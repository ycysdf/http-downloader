use std::collections::HashMap;
use std::future::Future;
use std::num::{NonZeroU8, NonZeroUsize};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use futures_util::{FutureExt, StreamExt};
use futures_util::future::{BoxFuture, OptionFuture};
use futures_util::stream::FuturesUnordered;
use reqwest::Request;
use tokio::fs::File;
use tokio::sync;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{chunk_item::ChunkItem, ChunkIterator, ChunkRange, DownloadError};
use crate::{DownloadedLenChangeNotify, DownloadingEndCause};

#[allow(dead_code)]
#[cfg_attr(
feature = "async-graphql",
derive(async_graphql::SimpleObject),
graphql(complex)
)]
pub struct ChunksInfo {
    finished_chunks: Vec<ChunkRange>,
    #[cfg_attr(feature = "async-graphql", graphql(skip))]
    downloading_chunks: Vec<Arc<ChunkItem>>,
    no_chunk_remaining: bool,
}

pub struct ChunkManager {
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    pub chunk_iterator: ChunkIterator,
    downloading_chunks: Mutex<HashMap<usize, Arc<ChunkItem>>>,
    download_connection_count_sender: sync::watch::Sender<u8>,
    pub download_connection_count_receiver: sync::watch::Receiver<u8>,
    client: reqwest::Client,
    cancel_token: CancellationToken,
    pub superfluities_connection_count: AtomicU8,
    pub etag: Option<headers::ETag>,
    pub retry_count: u8,
}

impl ChunkManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        download_connection_count: NonZeroU8,
        client: reqwest::Client,
        cancel_token: CancellationToken,
        downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
        chunk_iterator: ChunkIterator,
        etag: Option<headers::ETag>,
        retry_count: u8,
    ) -> Self {
        let (download_connection_count_sender, download_connection_count_receiver) =
            sync::watch::channel(download_connection_count.get());

        Self {
            downloaded_len_sender,
            chunk_iterator,
            downloading_chunks: Mutex::new(HashMap::new()),
            download_connection_count_sender,
            download_connection_count_receiver,
            client,
            cancel_token,
            superfluities_connection_count: AtomicU8::new(0),
            etag,
            retry_count,
        }
    }

    pub fn change_connection_count(
        &self,
        connection_count: NonZeroU8,
    ) -> Result<(), sync::watch::error::SendError<u8>> {
        self.download_connection_count_sender.send(connection_count.get())
    }

    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) {
        let mut guard = self.chunk_iterator.data.write();
        guard.remaining.chunk_size = chunk_size.get();
    }

    pub fn downloaded_len(&self) -> u64 {
        *self.downloaded_len_sender.borrow()
    }

    pub fn connection_count(&self) -> u8 {
        *self.download_connection_count_sender.borrow()
    }

    pub fn clone_request(request: &Request) -> Box<Request> {
        let mut req = Request::new(request.method().clone(), request.url().clone());
        *req.headers_mut() = request.headers().clone();
        *req.version_mut() = request.version();
        *req.timeout_mut() = request.timeout().map(Clone::clone);
        Box::new(req)
    }

    pub async fn start_download(
        &self,
        file: File,
        request: Box<Request>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        #[cfg(feature = "breakpoint-resume")]
        breakpoint_resume: Option<Arc<crate::BreakpointResume>>,
    ) -> Result<DownloadingEndCause, DownloadError> {
        enum RunFuture<'a> {
            DownloadConnectionCountChanged(BoxFuture<'a, (sync::watch::Receiver<u8>, u8)>),
            ChunkDownloadEnd {
                chunk_index: usize,
                future: BoxFuture<'a, Result<DownloadingEndCause, DownloadError>>,
            },
        }

        #[derive(Debug)]
        enum RunFutureResult {
            DownloadConnectionCountChanged {
                receiver: sync::watch::Receiver<u8>,
                download_connection_count: u8,
            },
            ChunkDownloadEnd {
                chunk_index: usize,
                result: Result<DownloadingEndCause, DownloadError>,
            },
        }

        impl Future for RunFuture<'_> {
            type Output = RunFutureResult;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match self.get_mut() {
                    RunFuture::DownloadConnectionCountChanged(future) => {
                        future.poll_unpin(cx).map(|r| RunFutureResult::DownloadConnectionCountChanged {
                            receiver: r.0,
                            download_connection_count: r.1,
                        })
                    }
                    RunFuture::ChunkDownloadEnd {
                        future,
                        chunk_index
                    } => {
                        future.poll_unpin(cx).map(|result| RunFutureResult::ChunkDownloadEnd {
                            chunk_index: chunk_index.clone(),
                            result,
                        })
                    }
                }
            }
        }

        let mut futures_unordered = FuturesUnordered::new();


        let file = Arc::new(Mutex::new(file));
        let download_next_chunk = || async {
            match self
                .download_next_chunk(
                    file.clone(),
                    downloaded_len_receiver.clone(),
                    Self::clone_request(&request),
                )
                .await {
                None => {
                    None
                }
                Some((chunk_index, future)) => {
                    Some(RunFuture::ChunkDownloadEnd {
                        chunk_index,
                        future: future.boxed(),
                    })
                }
            }
        };
        match download_next_chunk().await {
            None => {
                #[cfg(feature = "tracing")]
                tracing::trace!("No Chunk!");
                return Ok(DownloadingEndCause::DownloadFinished);
            }
            Some(future) => futures_unordered.push(future)
        }

        let mut is_iter_finished = false;
        for _ in 0..(self.connection_count() - 1) {
            match download_next_chunk().await {
                None => {
                    is_iter_finished = true;
                    break;
                }
                Some(future) => futures_unordered.push(future)
            }
        }
        futures_unordered.push(RunFuture::DownloadConnectionCountChanged({
            let mut receiver = self.download_connection_count_receiver.clone();
            async move {
                let _ = receiver.changed().await;
                let i = *receiver.borrow();
                (receiver, i)
            }.boxed()
        }));

        #[cfg(feature = "breakpoint-resume")]
            let save_data = || async {
            if let Some(notifies) = breakpoint_resume.as_ref() {
                #[cfg(feature = "tracing")]
                    let span = tracing::info_span!("Archive Data");
                #[cfg(feature = "tracing")]
                    let _ = span.enter();
                let notified = notifies.archive_complete_notify.notified();
                notifies.data_archive_notify.notify_one();
                notified.await;
            }
        };

        let mut result = Result::<DownloadingEndCause, DownloadError>::Ok(DownloadingEndCause::DownloadFinished);
        while let Some(future_result) = futures_unordered.next().await {
            match future_result {
                RunFutureResult::DownloadConnectionCountChanged {
                    download_connection_count,
                    mut receiver
                } => {
                    if download_connection_count == 0 {
                        continue;
                    }

                    let current_count = self.get_chunks().await.len();
                    let diff = download_connection_count as i16 - current_count as i16;
                    if diff >= 0 {
                        self.superfluities_connection_count
                            .store(0, Ordering::SeqCst);
                        for _ in 0..diff {
                            match download_next_chunk().await {
                                None => {
                                    is_iter_finished = true;
                                    break;
                                }
                                Some(future) => futures_unordered.push(future)
                            }
                        }
                    } else {
                        self.superfluities_connection_count
                            .store(diff.unsigned_abs() as u8, Ordering::SeqCst);
                    }

                    futures_unordered.push(RunFuture::DownloadConnectionCountChanged(async move {
                        let _ = receiver.changed().await;
                        let i = *receiver.borrow();
                        (receiver, i)
                    }.boxed()))
                }
                RunFutureResult::ChunkDownloadEnd {
                    chunk_index,
                    result: Ok(DownloadingEndCause::DownloadFinished)
                } => {
                    let (downloading_chunk_count, _) = self.remove_chunk(chunk_index).await;

                    #[cfg(feature = "breakpoint-resume")]
                    save_data().await;
                    if is_iter_finished {
                        if downloading_chunk_count == 0 {
                            debug_assert_eq!(
                                self.chunk_iterator.content_length,
                                *self.downloaded_len_sender.borrow()
                            );
                            break;
                        }
                    } else if self.superfluities_connection_count.load(Ordering::SeqCst) == 0 {
                        match download_next_chunk().await {
                            None => {
                                is_iter_finished = true;
                                if downloading_chunk_count == 0 {
                                    debug_assert_eq!(
                                        self.chunk_iterator.content_length,
                                        *self.downloaded_len_sender.borrow()
                                    );
                                    break;
                                }
                            }
                            Some(future) => futures_unordered.push(future)
                        }
                    } else {
                        self.superfluities_connection_count
                            .fetch_sub(1, Ordering::SeqCst);
                    }
                }
                RunFutureResult::ChunkDownloadEnd {
                    result: Err(err),
                    ..
                } => {
                    // 只记录第一个错误
                    if matches!(result,Ok(DownloadingEndCause::DownloadFinished)) {
                        result = Err(err);
                        // 取消监听 连接数 的更改
                        let _ =
                            self.download_connection_count_sender.send(0);
                        // 取消其他的 Chunk 下载
                        self.cancel_token.cancel();
                    }
                }
                RunFutureResult::ChunkDownloadEnd {
                    result: Ok(DownloadingEndCause::Cancelled),
                    ..
                } => {
                    if matches!(result,Ok(DownloadingEndCause::DownloadFinished)) {
                        result = Ok(DownloadingEndCause::Cancelled);
                        // 取消监听 连接数 的更改
                        let _ =
                            self.download_connection_count_sender.send(0);
                    }
                }
            }
        }
        // 如果没有完成，怎保存进度
        if !matches!(result,Ok(DownloadingEndCause::DownloadFinished)) {
            #[cfg(feature = "breakpoint-resume")]
            save_data().await;
        }
        result
    }
    async fn insert_chunk(&self, item: Arc<ChunkItem>) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        downloading_chunks.insert(item.chunk_info.index, item);
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        let mut downloading_chunks: Vec<_> = self
            .downloading_chunks
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        downloading_chunks.sort_by(|a, b| a.chunk_info.range.start.cmp(&b.chunk_info.range.start));
        downloading_chunks
    }

    pub async fn get_chunks_info(&self) -> ChunksInfo {
        let downloading_chunks = self.get_chunks().await;
        let mut finished_chunks = vec![];

        let no_chunk_remaining = self.chunk_iterator.data.read().no_chunk_remaining();
        if !downloading_chunks.is_empty() {
            let first_start = downloading_chunks[0].chunk_info.range.start;
            if first_start != 0 {
                finished_chunks.push(ChunkRange::new(0, first_start - 1));
            }
            for (index, _) in downloading_chunks.iter().enumerate() {
                if index == downloading_chunks.len() - 1 {
                    break;
                }

                let start = downloading_chunks[index].chunk_info.range.end;
                let end = downloading_chunks[index + 1].chunk_info.range.start;
                if (end - start) != 1 {
                    finished_chunks.push(ChunkRange::new(start + 1, end - 1));
                }
            }
            if no_chunk_remaining {
                let last = downloading_chunks.last().unwrap();
                if last.chunk_info.range.end != self.chunk_iterator.content_length - 1 {
                    finished_chunks.push(ChunkRange::new(
                        last.chunk_info.range.end + 1,
                        self.chunk_iterator.content_length - 1,
                    ))
                }
            }
        }
        ChunksInfo {
            downloading_chunks,
            finished_chunks,
            no_chunk_remaining,
        }
    }

    async fn remove_chunk(&self, index: usize) -> (usize, Option<Arc<ChunkItem>>) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        let removed = downloading_chunks.remove(&index);
        (downloading_chunks.len(), removed)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download_next_chunk(
        &self,
        file: Arc<Mutex<File>>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        request: Box<Request>,
    ) -> Option<(usize, impl Future<Output=Result<DownloadingEndCause, DownloadError>>)> {
        if let Some(chunk_info) = self.chunk_iterator.next() {
            let chunk_item = Arc::new(ChunkItem::new(
                chunk_info,
                self.cancel_token.child_token(),
                self.client.clone(),
                file,
                self.etag.clone(),
            ));
            self.insert_chunk(chunk_item.clone()).await;
            Some((chunk_item.chunk_info.index, chunk_item.download_chunk(request, self.retry_count, Some(LenChangedNotify {
                notify: downloaded_len_receiver,
                downloaded_len_sender: self.downloaded_len_sender.clone(),
            }))))
        } else {
            None
        }
    }
}

pub struct LenChangedNotify {
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    notify: Option<Arc<dyn DownloadedLenChangeNotify>>,
}

impl DownloadedLenChangeNotify for LenChangedNotify {
    fn receive_len(&self, len: usize) -> OptionFuture<BoxFuture<()>> {
        self.downloaded_len_sender
            .send_modify(|n| *n += len as u64);
        if let Some(notify) = self.notify.as_ref() {
            notify.receive_len(len)
        } else {
            None.into()
        }
    }
}


#[cfg(feature = "async-graphql")]
pub struct DownloadChunkObject(pub Arc<ChunkItem>);

#[cfg(feature = "async-graphql")]
impl From<Arc<ChunkItem>> for DownloadChunkObject {
    fn from(value: Arc<ChunkItem>) -> Self {
        DownloadChunkObject(value)
    }
}

#[cfg(feature = "async-graphql")]
#[async_graphql::Object]
impl DownloadChunkObject {
    pub async fn index(&self) -> usize {
        self.0.chunk_info.index
    }
    pub async fn start(&self) -> u64 {
        self.0.chunk_info.range.start
    }
    pub async fn end(&self) -> u64 {
        self.0.chunk_info.range.end
    }
    pub async fn len(&self) -> u64 {
        self.0.chunk_info.range.len()
    }
    pub async fn downloaded_len(&self) -> u64 {
        self.0.downloaded_len.load(Ordering::Relaxed)
    }
}

#[cfg_attr(feature = "async-graphql", async_graphql::ComplexObject)]
impl ChunksInfo {
    #[cfg(feature = "async-graphql")]
    pub async fn downloading_chunks(&self) -> Vec<DownloadChunkObject> {
        self.downloading_chunks
            .iter()
            .cloned()
            .map(Into::into)
            .collect()
    }
}
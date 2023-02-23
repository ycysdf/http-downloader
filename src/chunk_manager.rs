use std::collections::HashMap;
use std::num::{NonZeroU8, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use reqwest::Request;
use tokio::{select, sync};
use tokio::fs::File;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    chunk_item::{ChunkItem, ChunkMessageInfo, ChunkMessageKind, DownloadedChunkItem},
    ChunkIterator, ChunkRange, DownloadError,
};
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

pub struct ChunkManager {
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    pub chunk_iterator: ChunkIterator,
    downloading_chunks: Mutex<HashMap<usize, DownloadedChunkItem>>,
    #[cfg(feature = "breakpoint-resume")]
    pub data_archive_notify: sync::Notify,
    #[cfg(feature = "breakpoint-resume")]
    pub archive_complete_notify: sync::Notify,
    download_connection_count_sender: sync::watch::Sender<NonZeroU8>,
    pub download_connection_count_receiver: sync::watch::Receiver<NonZeroU8>,
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
            sync::watch::channel(download_connection_count);

        #[cfg(feature = "breakpoint-resume")]
            let (data_archive_notify, archive_complete_notify) =
            (sync::Notify::new(), sync::Notify::new());

        Self {
            #[cfg(feature = "breakpoint-resume")]
            data_archive_notify,
            #[cfg(feature = "breakpoint-resume")]
            archive_complete_notify,
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
    ) -> Result<(), sync::watch::error::SendError<NonZeroU8>> {
        self.download_connection_count_sender.send(connection_count)
    }

    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) {
        let mut guard = self.chunk_iterator.data.write();
        guard.remaining.chunk_size = chunk_size.get();
    }

    pub fn downloaded_len(&self) -> u64 {
        *self.downloaded_len_sender.borrow()
    }

    pub fn connection_count(&self) -> u8 {
        self.download_connection_count_sender.borrow().get()
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
        file: Arc<Mutex<File>>,
        request: Box<Request>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        _breakpoint_resume: bool,
    ) -> Result<DownloadingEndCause, DownloadError> {
        let (chunk_message_sender, mut chunk_message_receiver) =
            sync::mpsc::channel::<ChunkMessageInfo>(64);

        let mut is_iter_all_chunk = !self
            .download_next_chunk(
                file.clone(),
                chunk_message_sender.clone(),
                downloaded_len_receiver.clone(),
                Self::clone_request(&request),
            )
            .await;
        if is_iter_all_chunk {
            #[cfg(feature = "tracing")]
            tracing::warn!("No Chunk!");
            return Ok(DownloadingEndCause::DownloadFinished);
        }

        for _ in 0..(self.connection_count() - 1) {
            is_iter_all_chunk = !self
                .download_next_chunk(
                    file.clone(),
                    chunk_message_sender.clone(),
                    downloaded_len_receiver.clone(),
                    Self::clone_request(&request),
                )
                .await;
            if is_iter_all_chunk {
                break;
            }
        }

        let connection_count_handle_future = {
            let mut download_connection_count_receiver =
                self.download_connection_count_receiver.clone();
            let chunk_message_sender = chunk_message_sender.clone();
            let file = file.clone();
            let downloaded_len_receiver = downloaded_len_receiver.clone();
            let request = Self::clone_request(&request);
            async move {
                while download_connection_count_receiver.changed().await.is_ok() {
                    let download_connection_count =
                        download_connection_count_receiver.borrow().get();
                    let current_count = self.get_chunks().await.len();
                    let diff = download_connection_count as i16 - current_count as i16;
                    if diff >= 0 {
                        self.superfluities_connection_count
                            .store(0, Ordering::SeqCst);
                        for _ in 0..diff {
                            if !self
                                .download_next_chunk(
                                    file.clone(),
                                    chunk_message_sender.clone(),
                                    downloaded_len_receiver.clone(),
                                    Self::clone_request(&request),
                                )
                                .await
                            {
                                break;
                            }
                        }
                    } else {
                        self.superfluities_connection_count
                            .store(diff.unsigned_abs() as u8, Ordering::SeqCst);
                    }
                }
                DownloadingEndCause::Cancelled
            }
        };

        let message_handle_future = async move {
            let chunk_message_sender = chunk_message_sender;
            while let Some(message_info) = chunk_message_receiver.recv().await {
                match message_info.kind {
                    ChunkMessageKind::DownloadFinished => {
                        #[cfg(feature = "tracing")]
                            let span = tracing::info_span!(
                            "Start Handle DownloadFinished",
                            chunk_inde = message_info.chunk_index
                        );
                        #[cfg(feature = "tracing")]
                            let _ = span.enter();
                        let (downloading_chunk_count, chunk_item) =
                            self.remove_chunk(message_info.chunk_index).await;
                        let _ = chunk_item
                            .ok_or(DownloadError::ChunkRemoveFailed(message_info.chunk_index))?;
                        if _breakpoint_resume {
                            self.save_spec_data().await;
                        }
                        if is_iter_all_chunk {
                            if downloading_chunk_count == 0 {
                                debug_assert_eq!(
                                    self.chunk_iterator.content_length,
                                    *self.downloaded_len_sender.borrow()
                                );
                                break;
                            }
                        } else if self.superfluities_connection_count.load(Ordering::SeqCst) == 0 {
                            is_iter_all_chunk = !self
                                .download_next_chunk(
                                    file.clone(),
                                    chunk_message_sender.clone(),
                                    downloaded_len_receiver.clone(),
                                    Self::clone_request(&request),
                                )
                                .await;
                        } else {
                            self.superfluities_connection_count
                                .fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    ChunkMessageKind::Error(err) => {
                        if _breakpoint_resume {
                            self.save_spec_data().await;
                        }
                        return Err(err);
                    }
                    ChunkMessageKind::DownloadCancelled => {
                        if _breakpoint_resume {
                            self.save_spec_data().await;
                        }
                    }
                    ChunkMessageKind::DownloadLenAppend(append_len) => {
                        self.downloaded_len_sender
                            .send_modify(|n| *n += append_len as u64);
                    }
                }
            }
            Result::<DownloadingEndCause, DownloadError>::Ok(DownloadingEndCause::DownloadFinished)
        };

        let cancellation_token = self.cancel_token.clone();
        let r = select! {
            r = connection_count_handle_future => {Ok(r)}
            r = message_handle_future => {r}
            _ = cancellation_token.cancelled() => {
                Ok(DownloadingEndCause::Cancelled)
            }
        };
        r
    }
    async fn insert_chunk(&self, item: DownloadedChunkItem) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        downloading_chunks.insert(item.chunk_info.index, item);
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        let mut downloading_chunks: Vec<_> = self
            .downloading_chunks
            .lock()
            .await
            .values()
            .map(|n| n.chunk_item.clone())
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

    async fn save_spec_data(&self) {
        #[cfg(feature = "breakpoint-resume")]
        {
            #[cfg(feature = "tracing")]
                let span = tracing::info_span!("Archive Data");
            #[cfg(feature = "tracing")]
                let _ = span.enter();
            let notified = self.archive_complete_notify.notified();
            self.data_archive_notify.notify_one();
            notified.await;
        }
    }

    async fn remove_chunk(&self, index: usize) -> (usize, Option<DownloadedChunkItem>) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        let removed = downloading_chunks.remove(&index);
        (downloading_chunks.len(), removed)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download_next_chunk(
        &self,
        file: Arc<Mutex<File>>,
        sender: sync::mpsc::Sender<ChunkMessageInfo>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        request: Box<Request>,
    ) -> bool {
        if let Some(chunk_info) = self.chunk_iterator.next() {
            let chunk_item = Arc::new(ChunkItem::new(
                chunk_info,
                self.cancel_token.child_token(),
                self.client.clone(),
                sender,
                file,
                downloaded_len_receiver,
                self.etag.clone(),
            ));
            let item = chunk_item.start_download(request, self.retry_count);
            self.insert_chunk(item).await;
            true
        } else {
            false
        }
    }
}

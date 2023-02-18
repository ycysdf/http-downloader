use std::collections::HashMap;
use std::num::{NonZeroU8, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use reqwest::Request;
use tokio::{select, sync};
use tokio::fs::File;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "tracing")]
use tracing::warn;

use crate::{chunk_item::{ChunkItem, ChunkMessageInfo, ChunkMessageKind, DownloadedChunkItem}, ChunkIterator, DownloadError};
use crate::{DownloadedLenChangeNotify, DownloadingEndCause};

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
    pub downloading_duration: AtomicU32,
}

impl ChunkManager {
    pub fn new(
        download_connection_count: NonZeroU8,
        client: reqwest::Client,
        cancel_token: CancellationToken,
        downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
        chunk_iterator: ChunkIterator,
        etag: Option<headers::ETag>,
        retry_count: u8,
        downloading_duration: u32,
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
            downloading_duration: AtomicU32::new(downloading_duration),
        }
    }

    pub fn change_connection_count(&self, connection_count: NonZeroU8) -> Result<(), sync::watch::error::SendError<NonZeroU8>> {
        self.download_connection_count_sender.send(connection_count)
    }

    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) {
        let mut guard = self.chunk_iterator.data.lock();
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
        breakpoint_resume: bool,
    ) -> Result<DownloadingEndCause, DownloadError> {
        let (chunk_message_sender, mut chunk_message_receiver) =
            sync::mpsc::channel::<ChunkMessageInfo>(64);
        let mut downloading_duration_instant = Instant::now();

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
            warn!("No Chunk!");
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
                // let mut previous_count = self.connection_count();
                while download_connection_count_receiver.changed().await.is_ok() {
                    let download_connection_count = download_connection_count_receiver.borrow().get();
                    let current_count = self.get_chunks().await.len();
                    let diff = download_connection_count as i16 - current_count as i16;
                    if diff >= 0 {
                        self.superfluities_connection_count
                            .store(0, Ordering::SeqCst);
                        for _ in 0..diff {
                            if !self.download_next_chunk(
                                file.clone(),
                                chunk_message_sender.clone(),
                                downloaded_len_receiver.clone(),
                                Self::clone_request(&request),
                            )
                                .await {
                                break;
                            }
                        }
                    } else {
                        self.superfluities_connection_count
                            .store(diff.unsigned_abs() as u8, Ordering::SeqCst);
                    }

                    // let cc = self.superfluities_connection_count.load(Ordering::SeqCst);
                    // let mut add_connection_count = current as i16 - previous_count as i16;
                    // previous_count = current;
                    // if add_connection_count > 0 {
                    //     if cc > 0 {
                    //         let d = add_connection_count - (cc as i16);
                    //         if d >= 0 {
                    //             add_connection_count = d;
                    //         } else {
                    //             info!("add_connection_count sub {}",add_connection_count);
                    //
                    //             self.superfluities_connection_count
                    //                 .fetch_sub(add_connection_count as u8, Ordering::SeqCst);
                    //         }
                    //     }
                    //     for _ in 0..add_connection_count {
                    //         info!("add_connection_count {}",add_connection_count);
                    //         self.download_next_chunk(
                    //             file.clone(),
                    //             chunk_message_sender.clone(),
                    //             downloaded_len_receiver.clone(),
                    //             Self::clone_request(&request),
                    //         )
                    //             .await;
                    //     }
                    // } else {
                    //     info!("add_connection_count.unsigned_abs() {}",add_connection_count.unsigned_abs());
                    //     self.superfluities_connection_count
                    //         .fetch_add(add_connection_count.unsigned_abs() as u8, Ordering::SeqCst);
                    // }
                    // info!("self.superfluities_connection_count {}",self.superfluities_connection_count.load(Ordering::SeqCst));
                }
                DownloadingEndCause::Cancelled
            }
        };

        let message_handle_future = async move {
            let chunk_message_sender = chunk_message_sender;
            while let Some(message_info) = chunk_message_receiver.recv().await {
                match message_info.kind {
                    ChunkMessageKind::DownloadFinished => {
                        let (downloading_chunk_count, chunk_item) = self.remove_chunk(message_info.chunk_index).await;
                        let _ = chunk_item.ok_or(DownloadError::ChunkRemoveFailed(message_info.chunk_index))?;
                        self.downloading_duration.fetch_add(downloading_duration_instant.elapsed().as_secs() as u32, Ordering::Relaxed);
                        downloading_duration_instant = Instant::now();
                        #[cfg(feature = "breakpoint-resume")]
                        {
                            if breakpoint_resume {
                                let notified = self.archive_complete_notify.notified();
                                self.data_archive_notify.notify_one();
                                notified.await;
                            }
                        }
                        if is_iter_all_chunk {
                            if downloading_chunk_count == 0 {
                                debug_assert_eq!(self.chunk_iterator.content_length, *self.downloaded_len_sender.borrow());
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
                        // if let DownloadError::HttpRequestFailed(_) = err {
                        //     warn!("Request error! {:?}",err);
                        //     if !self.restart_chunk(message_info.chunk_index, true, Self::clone_request(&request)).await? {
                        //         return Err(err);
                        //     }
                        // }
                        return Err(err);
                    }
                    ChunkMessageKind::DownloadCancelled => {
                        // // 如果取消的是单个 Chunk，则重新下载此 Chunk
                        // if !self.cancel_token.is_cancelled() {
                        //     self.restart_chunk(message_info.chunk_index, false, Self::clone_request(&request)).await?;
                        // } else {
                        //     return Result::<DownloadingEndCause, DownloadError>::Ok(DownloadingEndCause::Cancelled);
                        // }
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
        self.downloading_duration.fetch_add(downloading_duration_instant.elapsed().as_secs() as u32, Ordering::Relaxed);
        r
    }
    async fn insert_chunk(&self, item: DownloadedChunkItem) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        downloading_chunks.insert(item.chunk_info.index, item);
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        let downloading_chunks = self.downloading_chunks.lock().await;
        downloading_chunks
            .values()
            .map(|n| n.chunk_item.clone())
            .collect()
    }

    async fn remove_chunk(&self, index: usize) -> (usize, Option<DownloadedChunkItem>) {
        let mut downloading_chunks = self.downloading_chunks.lock().await;
        let removed = downloading_chunks.remove(&index);
        (downloading_chunks.len(), removed)
    }

    /*    async fn restart_chunk(&self, index: usize, error_retry: bool, request: Box<Request>) -> Result<bool, DownloadError> {
            if let Some(DownloadedChunkItem { chunk_item, .. }) = self.remove_chunk(index).await.1 {
                let retry_count = chunk_item.retry_count.load(Ordering::Relaxed);
                if !error_retry || retry_count < self.retry_count {
                    let item = chunk_item.start_download(request,);
                    item.chunk_item.retry_count.store(
                        if error_retry {
                            retry_count + 1
                        } else {
                            retry_count
                        },
                        Ordering::Relaxed,
                    );
                    self.insert_chunk(item).await;
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else {
                Err(DownloadError::ChunkRemoveFailed(index))
            }
        }
    */
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

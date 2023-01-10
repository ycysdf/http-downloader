use std::io::SeekFrom;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use headers::HeaderMapExt;
use reqwest::Request;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::select;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{ChunkInfo, DownloadError};

#[derive(Debug)]
pub enum ChunkMessageKind {
    DownloadFinished,
    DownloadCancelled,
    DownloadLenAppend(usize),
    Error(DownloadError),
}

#[derive(Debug)]
pub struct ChunkMessageInfo {
    pub chunk_index: usize,
    pub kind: ChunkMessageKind,
}

pub trait DownloadedLenChangeNotify: Send + Sync {
    fn receive_len(&self, len: usize) -> Option<BoxFuture<()>>;
}

pub struct ChunkItem {
    pub chunk_info: ChunkInfo,
    pub downloaded_len: AtomicU64,
    cancel_token: CancellationToken,
    client: reqwest::Client,
    sender: tokio::sync::mpsc::Sender<ChunkMessageInfo>,
    file: Arc<Mutex<File>>,
    etag: Option<headers::ETag>,
    downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
}

impl ChunkItem {
    pub fn new(
        chunk_info: ChunkInfo,
        cancel_token: CancellationToken,
        client: reqwest::Client,
        sender: tokio::sync::mpsc::Sender<ChunkMessageInfo>,
        file: Arc<Mutex<File>>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        etag: Option<headers::ETag>,
    ) -> Self {
        Self {
            downloaded_len: AtomicU64::new(0),
            cancel_token,
            client,
            chunk_info,
            file,
            downloaded_len_receiver,
            sender,
            etag,
        }
    }
    async fn send_message(
        &self,
        message_kind: ChunkMessageKind,
    ) -> Result<(), SendError<ChunkMessageInfo>> {
        self.sender
            .send(ChunkMessageInfo {
                chunk_index: self.chunk_info.index,
                kind: message_kind,
            })
            .await
    }

    #[inline]
    async fn add_downloaded_len(&self, len: usize) {
        self.downloaded_len.fetch_add(len as u64, Ordering::Relaxed);
        debug_assert!(self.downloaded_len.load(Ordering::SeqCst) <= self.chunk_info.range.len(), "downloaded_len:{},chunk_info.range.len():{}", self.downloaded_len.load(Ordering::SeqCst), self.chunk_info.range.len());
        self.send_message(ChunkMessageKind::DownloadLenAppend(len))
            .await
            .unwrap_or_else(|err| {
                #[cfg(feature = "tracing")]
                tracing::warn!("ChunkMessageInfoSendFailed! {:?}",err);
            });
    }

    async fn download_chunk(self: Arc<Self>, request: Box<Request>, retry_count: u8) -> Result<bool, DownloadError> {
        let cancel_token = self.cancel_token.clone();
        let mut chunk_bytes = Vec::with_capacity(self.chunk_info.range.len() as usize);

        let mut cur_retry_count = 0;
        let future = async {
            let response = self
                .client
                .execute(*request)
                .await?;
            if self.etag.is_some() {
                let etag = response.headers().typed_get::<headers::ETag>();
                if etag != self.etag {
                    tracing::error!("current etag: {:?} , target etag:{:?}", self.etag, etag);
                    return Err(DownloadError::ServerFileAlreadyChanged);
                }
            }
            let mut stream = response.bytes_stream();
            while let Some(bytes) = stream.next().await {
                let bytes: Bytes = {
                    match bytes {
                        Ok(bytes) => { bytes }
                        Err(err) => {
                            cur_retry_count += 1;
                            #[cfg(feature = "tracing")]
                            tracing::warn!("Request error! {:?}",err);
                            if cur_retry_count > retry_count {
                                return Err(DownloadError::HttpRequestFailed(err));
                            }
                            continue;
                        }
                    }
                };
                let len = bytes.len();
                chunk_bytes.extend(bytes);
                self.add_downloaded_len(len).await;
                if let Some(downloaded_len_receiver) = self.downloaded_len_receiver.as_ref() {
                    match downloaded_len_receiver.receive_len(len) {
                        None => {}
                        Some(r) => r.await,
                    };
                }
            }
            Result::<(), DownloadError>::Ok(())
        };

        select! {
            r = future => {
                r?;
                let mut file = self.file.lock().await;
                file.seek(SeekFrom::Start(self.chunk_info.range.start)).await?;
                debug_assert_eq!(chunk_bytes.len() as u64,self.chunk_info.range.len());
                file.write_all(chunk_bytes.as_ref()).await?;
                file.flush().await?;
                file.sync_all().await?;
                Ok(true)
            }
            _ = cancel_token.cancelled() => {
                let mut file = self.file.lock().await;
                file.seek(SeekFrom::Start(self.chunk_info.range.start)).await?;
                debug_assert!(chunk_bytes.len() as u64 <= self.chunk_info.range.len(),"chunk_bytes.len() = {}, self.chunk_info.range.len() = {}", chunk_bytes.len(), self.chunk_info.range.len());
                file.write_all(chunk_bytes.as_ref()).await?;
                file.flush().await?;
                file.sync_all().await?;
                Ok(false)
            }
        }
    }
    pub fn start_download(self: Arc<Self>, mut request: Box<Request>, retry_count: u8) -> DownloadedChunkItem {
        use futures_util::FutureExt;
        let chunk_item = self.clone();
        request.headers_mut().typed_insert(self.chunk_info.range.to_range_header());
        let join_handle =
            tokio::spawn(self.clone().download_chunk(request, retry_count).then(|result| async move {
                match result {
                    Ok(is_finished) => {
                        if is_finished {
                            self.send_message(ChunkMessageKind::DownloadFinished).await.unwrap_or_else(|err| {
                                #[cfg(feature = "tracing")]
                                tracing::warn!("ChunkMessageInfoSendFailed! {:?}",err);
                            })
                        }
                    }
                    Err(err) => {
                        self.send_message(ChunkMessageKind::Error(err)).await.unwrap_or_else(|err| {
                            #[cfg(feature = "tracing")]
                            tracing::warn!("ChunkMessageInfoSendFailed! {:?}",err);
                        })
                    }
                };
            }));
        DownloadedChunkItem::new(chunk_item, join_handle)
    }
}

pub struct DownloadedChunkItem {
    pub chunk_item: Arc<ChunkItem>,
    pub join_handle: JoinHandle<()>,
}

impl DownloadedChunkItem {
    pub fn new(chunk_item: Arc<ChunkItem>, join_handle: JoinHandle<()>) -> Self {
        Self {
            chunk_item,
            join_handle,
        }
    }
}

impl Deref for DownloadedChunkItem {
    type Target = Arc<ChunkItem>;

    fn deref(&self) -> &Self::Target {
        &self.chunk_item
    }
}

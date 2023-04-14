use std::fmt::Debug;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Response;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::select;
use tokio::sync;
use tokio_util::sync::CancellationToken;

use crate::{ChunkManager, DownloadArchiveData, DownloadedLenChangeNotify, DownloadError, DownloadingEndCause, HttpDownloadConfig};

#[derive(Debug)]
pub struct SingleDownload {
    cancel_token: CancellationToken,
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    pub content_length: Option<u64>,
}

impl SingleDownload {
    pub fn new(
        cancel_token: CancellationToken,
        downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            cancel_token,
            downloaded_len_sender,
            content_length,
        }
    }

    pub async fn download(
        &self,
        mut file: File,
        response: Box<Response>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        buffer_size: usize,
    ) -> Result<DownloadingEndCause, DownloadError> {
        use futures_util::StreamExt;
        let mut chunk_bytes = Vec::with_capacity(buffer_size);
        let future = async {
            let mut stream = response.bytes_stream();
            while let Some(bytes) = stream.next().await {
                let bytes: Bytes = {
                    // 因为无法断点续传，所以无法重试
                    match bytes {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            return Err(DownloadError::HttpRequestFailed(err));
                        }
                    }
                };
                let len = bytes.len();

                // 超过缓冲大小就写入磁盘
                if chunk_bytes.len() + len > chunk_bytes.capacity() {
                    file.write_all(&chunk_bytes).await?;
                    file.flush().await?;
                    file.sync_all().await?;
                    chunk_bytes.clear();
                }

                chunk_bytes.extend(bytes);
                self.downloaded_len_sender.send_modify(|n| *n += len as u64);
                if let Some(downloaded_len_receiver) = downloaded_len_receiver.as_ref() {
                    downloaded_len_receiver.receive_len(len).await;
                }
            }
            Result::<(), DownloadError>::Ok(())
        };
        Ok(select! {
            r = future => {
                r?;
                file.write_all(&chunk_bytes).await?;
                file.flush().await?;
                file.sync_all().await?;
                DownloadingEndCause::DownloadFinished
            }
            _ = self.cancel_token.cancelled() => {DownloadingEndCause::Cancelled}
        })
    }
}

#[async_trait]
pub trait ResponseHandler {
    fn new(
        config: &Arc<HttpDownloadConfig>,
        cancel_token: CancellationToken,
        archive_data: Option<Box<DownloadArchiveData>>,
        content_length: u64,
        client: reqwest::Client,
        downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    ) -> Self;

    async fn download(
        &self,
        file: File,
        response: Box<Response>,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
        config: &Arc<HttpDownloadConfig>,
        redirection_location: Option<String>,
    ) -> Result<DownloadingEndCause, DownloadError>;
}

pub enum DownloadWay {
    Ranges(Arc<ChunkManager>),
    Single(SingleDownload),
}

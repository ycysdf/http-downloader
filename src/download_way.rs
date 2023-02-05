use std::fmt::Debug;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use reqwest::Response;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::select;
use tokio::sync;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{ChunkManager, DownloadedLenChangeNotify, DownloadError, DownloadingEndCause};

#[derive(Debug)]
pub struct SingleDownload {
    cancel_token: CancellationToken,
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    content_length: Option<u64>,
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
            content_length
        }
    }

    pub async fn download(
        &self,
        file: Arc<Mutex<File>>,
        response: Box<Response>,
        retry_count: u8,
        downloaded_len_receiver: Option<Arc<dyn DownloadedLenChangeNotify>>,
    ) -> Result<DownloadingEndCause, DownloadError> {
        let mut chunk_bytes = Vec::with_capacity(1024 * 1024);
        use futures_util::StreamExt;
        let mut cur_retry_count = 0;
        let future = async {
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

                if chunk_bytes.len() + len > chunk_bytes.capacity() {
                    let mut file = file.lock().await;
                    file.write_all(&chunk_bytes).await?;
                    file.flush().await?;
                    file.sync_all().await?;
                    chunk_bytes.clear();
                }

                chunk_bytes.extend(bytes);
                self.downloaded_len_sender.send_modify(|n| *n += len as u64);
                if let Some(downloaded_len_receiver) = downloaded_len_receiver.as_ref() {
                    match downloaded_len_receiver.receive_len(len) {
                        None => {}
                        Some(r) => r.await,
                    };
                }
            }
            Result::<(), DownloadError>::Ok(())
        };
        Ok(select! {
            r = future => {
                r?;
                let mut file = file.lock().await;
                file.write_all(&chunk_bytes).await?;
                file.flush().await?;
                file.sync_all().await?;
                DownloadingEndCause::DownloadFinished
            }
            _ = self.cancel_token.cancelled() => {DownloadingEndCause::Cancelled}
        })
    }
}

pub enum DownloadWay {
    Ranges(Arc<ChunkManager>),
    Single(SingleDownload),
}

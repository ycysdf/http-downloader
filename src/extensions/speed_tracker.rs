use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::FutureExt;
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use tokio::{select, sync};

use crate::{DownloaderWrapper, DownloadExtensionInstance, DownloadFuture, DownloadStartError, DownloadWay, HttpFileDownloader};

#[derive(Default)]
pub struct DownloadSpeedTrackerExtension {
    pub log: bool,
}

impl DownloadSpeedTrackerExtension {
    pub fn new() -> Self {
        Self::default()
    }
}

pub struct DownloadSpeedTrackerState {
    pub receiver: sync::watch::Receiver<u64>,
}

impl DownloadSpeedTrackerState {
    pub fn download_speed(&self) -> u64 {
        *self.receiver.borrow()
    }

    #[cfg(feature = "async-stream")]
    pub async fn stream(&self) -> impl Stream<Item=u64> {
        let mut receiver = self.receiver.clone();
        async_stream::stream! {
            let download_speed = *receiver.borrow();
            yield download_speed;

            while receiver.changed().await.is_ok() {
            let download_speed = *receiver.borrow();
              yield download_speed;
            }
        }
    }
}

pub struct DownloadSpeedExtensionInstance {
    downloaded_len_receiver: sync::watch::Receiver<u64>,
    download_speed_sender: Arc<sync::watch::Sender<u64>>,
    log: bool,
}


impl DownloadExtensionInstance for DownloadSpeedExtensionInstance {
    type ExtensionParam = DownloadSpeedTrackerExtension;
    type ExtensionState = DownloadSpeedTrackerState;

    fn new(param: Self::ExtensionParam, downloader: &mut HttpFileDownloader) -> (Self, Self::ExtensionState) where Self: Sized {
        let DownloadSpeedTrackerExtension { log } = param;
        let (sender, receiver) = sync::watch::channel(0);
        let downloaded_len_receiver = downloader.downloaded_len_receiver.clone();
        (
            DownloadSpeedExtensionInstance {
                download_speed_sender: Arc::new(sender),
                downloaded_len_receiver,
                log,
            },
            DownloadSpeedTrackerState { receiver },
        )
    }
}

#[async_trait]
impl DownloaderWrapper for DownloadSpeedExtensionInstance {
    async fn download(
        &mut self,
        downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError> {
        let (sender, download_way_receiver) = sync::oneshot::channel();
        downloader.downloading_state_oneshot_vec.push(sender);

        // let last_downloaded_len = params.archive_data.as_ref().map(|n| n.downloaded_len).unwrap_or(0);
        let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
        let downloaded_len_sender = self.download_speed_sender.clone();
        let log = self.log;


        let future = async move {
            let download_way_receiver = download_way_receiver
                .await
                .map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;

            let mut instant = Instant::now();
            let mut last_downloaded_len = if let DownloadWay::Ranges(chunk_manager) = &download_way_receiver.download_way {
                chunk_manager.downloaded_len()
            } else {
                0
            };
            loop {
                downloaded_len_receiver.changed().await.map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;
                if instant.elapsed().as_millis() > 1000 {
                    let downloaded_len = *downloaded_len_receiver.borrow();
                    let value = downloaded_len.max(last_downloaded_len) - last_downloaded_len;
                    instant = Instant::now();
                    #[cfg(feature = "tracing")]
                    if log {
                        let value = value as f64 / 1024_f64;
                        if value > 1024_f64 {
                            tracing::info!("Download speed: {:.2} Mb/s", value / 1024_f64);
                        } else {
                            tracing::info!("Download speed: {:.2} Kb/s", value);
                        }
                    }
                    let _ = downloaded_len_sender.send(value);
                    last_downloaded_len = downloaded_len;
                }
            }
        };
        let downloaded_len_sender = self.download_speed_sender.clone();

        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    let _ = downloaded_len_sender.send(0);
                    r
                }
            }
        }.boxed())
    }
}

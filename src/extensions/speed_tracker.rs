use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::FutureExt;
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use tokio::{select, sync};

use crate::{DownloaderWrapper, DownloadExtensionBuilder, DownloadFuture, DownloadingState, DownloadStartError, DownloadWay, HttpFileDownloader};

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
    pub fn stream(&self) -> impl Stream<Item=u64> + 'static {
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

pub struct DownloadSpeedDownloaderWrapper {
    downloaded_len_receiver: sync::watch::Receiver<u64>,
    download_speed_sender: Arc<sync::watch::Sender<u64>>,
    downloading_state_receiver: Option<sync::oneshot::Receiver<Arc<DownloadingState>>>,
    log: bool,
}


impl DownloadExtensionBuilder for DownloadSpeedTrackerExtension {
    type Wrapper = DownloadSpeedDownloaderWrapper;
    type ExtensionState = DownloadSpeedTrackerState;

    fn build(self, downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) where Self: Sized {
        let DownloadSpeedTrackerExtension { log } = self;
        let (sender, receiver) = sync::watch::channel(0);
        let downloaded_len_receiver = downloader.downloaded_len_receiver.clone();
        (
            DownloadSpeedDownloaderWrapper {
                download_speed_sender: Arc::new(sender),
                downloaded_len_receiver,
                log,
                downloading_state_receiver: None,
            },
            DownloadSpeedTrackerState { receiver },
        )
    }
}

impl DownloaderWrapper for DownloadSpeedDownloaderWrapper {
    fn prepare_download(&mut self, downloader: &mut HttpFileDownloader) -> Result<(), DownloadStartError> {
        let (sender, download_way_receiver) = sync::oneshot::channel();
        downloader.downloading_state_oneshot_vec.push(sender);
        self.downloading_state_receiver = Some(download_way_receiver);
        Ok(())
    }

    fn download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError> {
        let downloading_state_receiver = self.downloading_state_receiver.take().unwrap();

        let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
        let download_speed_sender = self.download_speed_sender.clone();
        let log = self.log;


        let future = async move {
            let download_way_receiver = downloading_state_receiver
                .await
                .map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;

            let mut last_downloaded_len = if let DownloadWay::Ranges(chunk_manager) = &download_way_receiver.download_way {
                chunk_manager.downloaded_len()
            } else {
                0
            };
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let downloaded_len = *downloaded_len_receiver.borrow();
                let value = downloaded_len.max(last_downloaded_len) - last_downloaded_len;
                #[cfg(feature = "tracing")]
                if log {
                    let value = value as f64 / 1024_f64;
                    if value > 1024_f64 {
                        tracing::info!("Download speed: {:.2} Mb/s", value / 1024_f64);
                    } else {
                        tracing::info!("Download speed: {:.2} Kb/s", value);
                    }
                }
                let _ = download_speed_sender.send(value);
                last_downloaded_len = downloaded_len;
            }
        };
        let download_speed_sender = self.download_speed_sender.clone();

        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    let _ = download_speed_sender.send(0);
                    r
                }
            }
        }.boxed())
    }
}

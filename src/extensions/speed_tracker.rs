use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use tokio::{select, sync};

use crate::{DownloadController, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, DownloadWay, HttpFileDownloader};

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

impl<DC: DownloadController> DownloadExtension<DC> for DownloadSpeedTrackerExtension {
    type DownloadController = DownloadSpeedExtensionController<DC>;
    type ExtensionState = DownloadSpeedTrackerState;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        let DownloadSpeedTrackerExtension { log } = self;
        let (sender, receiver) = sync::watch::channel(0);
        let downloaded_len_receiver = downloader.downloaded_len_receiver.clone();

        (
            Arc::new(DownloadSpeedExtensionController {
                inner,
                download_speed_sender: Arc::new(sender),
                downloaded_len_receiver,
                log,
            }),
            DownloadSpeedTrackerState { receiver },
        )
    }
}

pub struct DownloadSpeedExtensionController<DC: DownloadController> {
    inner: Arc<DC>,
    downloaded_len_receiver: sync::watch::Receiver<u64>,
    download_speed_sender: Arc<sync::watch::Sender<u64>>,
    log: bool,
}

#[async_trait]
impl<DC: DownloadController> DownloadController for DownloadSpeedExtensionController<DC> {
    async fn download(
        self: Arc<Self>,
        mut params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        let (sender, download_way_receiver) = sync::oneshot::channel();
        params.download_way_oneshot_vec.push(sender);

        // let last_downloaded_len = params.archive_data.as_ref().map(|n| n.downloaded_len).unwrap_or(0);
        let download_future = self.inner.to_owned().download(params).await?;

        let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
        let downloaded_len_sender = self.download_speed_sender.clone();
        let log = self.log;


        let future = async move {
            let download_way_receiver = download_way_receiver
                .await
                .map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;

            let mut instant = Instant::now();
            let mut last_downloaded_len = if let DownloadWay::Ranges(chunk_manager) = download_way_receiver.as_ref() {
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

        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {r}
            }
        }.boxed())
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.inner.cancel().await
    }
}

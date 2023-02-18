use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::sync;
#[cfg(feature = "tracing")]
use tracing::{error, info};

use crate::{DownloadController, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, HttpFileDownloader};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkItemPendingType {
    QueueUp,
    Starting,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloaderStatus {
    NoStart,
    Running,
    Pending(NetworkItemPendingType),
    Error(String),
    Finished,
}

impl DownloaderStatus {
    pub fn is_end(&self) -> bool {
        matches!(
            self,
            DownloaderStatus::NoStart | DownloaderStatus::Error(_) | DownloaderStatus::Finished
        )
    }
}

pub struct DownloadStatusSender {
    pub log: bool,
    pub status_sender: sync::watch::Sender<DownloaderStatus>,
}

impl DownloadStatusSender {
    pub fn change_status(&self, status: DownloaderStatus) {
        #[cfg(feature = "tracing")]
        if self.log {
            info!("Status changed {:?}", &status);
        }
        self.status_sender.send(status).unwrap_or_else(|err| {
            #[cfg(feature = "tracing")]
            error!("Send download status failed! {:?}", err);
        });
    }
}

pub struct DownloadStatusTrackerState {
    pub status_sender: Arc<DownloadStatusSender>,
    pub status_receiver: sync::watch::Receiver<DownloaderStatus>,
}

impl DownloadStatusTrackerState {
    pub fn status(&self) -> DownloaderStatus {
        self.status_receiver.borrow().clone()
    }
}

pub struct DownloadStatusTrackerExtension {
    pub log: bool,
}

impl<DC: DownloadController> DownloadExtension<DC> for DownloadStatusTrackerExtension {
    type DownloadController = DownloadStatusTrackerController<DC>;
    type ExtensionState = DownloadStatusTrackerState;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        drop(downloader);
        let (status_sender, status_receiver) = sync::watch::channel(DownloaderStatus::NoStart);
        let status_sender = Arc::new(DownloadStatusSender {
            log: self.log,
            status_sender,
        });
        (
            Arc::new(DownloadStatusTrackerController {
                inner,
                status_receiver: status_receiver.clone(),
                status_sender: status_sender.clone(),
            }),
            DownloadStatusTrackerState {
                status_receiver,
                status_sender,
            },
        )
    }
}

pub struct DownloadStatusTrackerController<DC: DownloadController> {
    inner: Arc<DC>,
    pub status_sender: Arc<DownloadStatusSender>,
    status_receiver: sync::watch::Receiver<DownloaderStatus>,
}

impl<DC: DownloadController> DownloadStatusTrackerController<DC> {
    pub fn status(&self) -> DownloaderStatus {
        self.status_receiver.borrow().clone()
    }
}

#[async_trait]
impl<DC: DownloadController> DownloadController for DownloadStatusTrackerController<DC> {
    async fn download(
        self: Arc<Self>,
        params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        match self.status() {
            DownloaderStatus::Running => return Err(DownloadStartError::AlreadyDownloading),
            DownloaderStatus::Pending(pending_type) => {
                match pending_type {
                    NetworkItemPendingType::Starting => {
                        return Err(DownloadStartError::Starting);
                    }
                    NetworkItemPendingType::Stopping => {
                        return Err(DownloadStartError::Stopping);
                    }
                    _ => {}
                }
            }
            DownloaderStatus::Finished => {
                #[cfg(feature = "tracing")]
                tracing::trace!("Restart download!");
                // return Err(DownloadStartError::AlreadyDownloadFinished)
            }
            _ => {}
        }
        let status_sender = self.status_sender.clone();
        status_sender.change_status(DownloaderStatus::Pending(NetworkItemPendingType::Starting));
        match self.inner.to_owned().download(params).await {
            Ok(receiver) => {
                status_sender.change_status(DownloaderStatus::Running);
                Ok(async move {
                    let r = receiver.await;
                    (match &r {
                        Ok(end_cause) => match end_cause {
                            DownloadingEndCause::DownloadFinished => {
                                status_sender.change_status(DownloaderStatus::Finished)
                            }
                            DownloadingEndCause::Cancelled => {
                                status_sender.change_status(DownloaderStatus::NoStart)
                            }
                        },
                        Err(err) => status_sender.change_status(DownloaderStatus::Error(err.to_string())),
                    });
                    r
                }
                    .boxed())
            }
            Err(err) => {
                status_sender.change_status(DownloaderStatus::Error(err.to_string()));
                Err(err)
            }
        }
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.status_sender.change_status(DownloaderStatus::Pending(NetworkItemPendingType::Stopping));
        self.inner.cancel().await?;
        Ok(())
    }
}

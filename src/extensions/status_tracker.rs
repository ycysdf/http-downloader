use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::sync;
#[cfg(feature = "tracing")]
use tracing::{error, info};

use crate::{DownloadController, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, HttpFileDownloader};

#[derive(Debug, Clone,PartialEq, Eq)]
pub enum NetworkItemPendingType {
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

pub struct DownloadStatusTrackerState {
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

        (
            Arc::new(DownloadStatusTrackerController {
                inner,
                log: self.log,
                status_receiver: status_receiver.clone(),
                status_sender: Arc::new(status_sender),
            }),
            DownloadStatusTrackerState { status_receiver },
        )
    }
}

pub struct DownloadStatusTrackerController<DC: DownloadController> {
    inner: Arc<DC>,
    log: bool,
    status_receiver: sync::watch::Receiver<DownloaderStatus>,
    status_sender: Arc<sync::watch::Sender<DownloaderStatus>>,
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
                return Err(DownloadStartError::Pending(pending_type));
            }
            DownloaderStatus::Finished => {
                #[cfg(feature = "tracing")]
                tracing::trace!("Restart download!");
                // return Err(DownloadStartError::AlreadyDownloadFinished)
            }
            _ => {}
        }
        let status_sender = self.status_sender.clone();
        let log = self.log;
        let change_status = move |status: DownloaderStatus| {
            #[cfg(feature = "tracing")]
            if log {
                info!("Status changed {:?}", &status);
            }
            status_sender.send(status).unwrap_or_else(|err| {
                #[cfg(feature = "tracing")]
                error!("Send download status failed! {:?}", err);
            });
        };
        change_status(DownloaderStatus::Pending(NetworkItemPendingType::Starting));
        match self.inner.to_owned().download(params).await {
            Ok(receiver) => {
                change_status(DownloaderStatus::Running);
                Ok(async move {
                    let r = receiver.await;
                    (match &r {
                        Ok(end_cause) => match end_cause {
                            DownloadingEndCause::DownloadFinished => {
                                change_status(DownloaderStatus::Finished)
                            }
                            DownloadingEndCause::Cancelled => {
                                change_status(DownloaderStatus::NoStart)
                            }
                        },
                        Err(err) => change_status(DownloaderStatus::Error(err.to_string())),
                    });
                    r
                }
                    .boxed())
            }
            Err(err) => {
                change_status(DownloaderStatus::Error(err.to_string()));
                Err(err)
            }
        }
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        {
            let status = DownloaderStatus::Pending(NetworkItemPendingType::Stopping);
            #[cfg(feature = "tracing")]
            if self.log {
                info!("Status changed {:?}", &status);
            }
            self.status_sender.send(status).unwrap_or_else(|err| {
                #[cfg(feature = "tracing")]
                error!("Send download status failed! {:?}", err);
            });
        }
        self.inner.cancel().await?;
        Ok(())
    }
}

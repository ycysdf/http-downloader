use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::FutureExt;
use tokio::{select, sync};

use crate::{DownloadCancelFuture, DownloaderWrapper, DownloadExtensionInstance, DownloadFuture, DownloadingEndCause, DownloadStartError, DownloadStopError, HttpFileDownloader};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkItemPendingType {
    QueueUp,
    Starting,
    Stopping,
    Initializing,
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
            tracing::info!("Status changed {:?}", &status);
        }
        self.status_sender.send(status).unwrap_or_else(|err| {
            #[cfg(feature = "tracing")]
            tracing::error!("Send download status failed! {:?}", err);
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

impl DownloadStatusTrackerExtension {
    pub fn new() -> Self {
        Self {
            log: false,
        }
    }
}

pub struct DownloadStatusTrackerController {
    pub status_sender: Arc<DownloadStatusSender>,
    status_receiver: sync::watch::Receiver<DownloaderStatus>,
}

impl DownloadStatusTrackerController {
    pub fn status(&self) -> DownloaderStatus {
        self.status_receiver.borrow().clone()
    }
}

impl DownloadExtensionInstance for DownloadStatusTrackerController {
    type ExtensionParam = DownloadStatusTrackerExtension;
    type ExtensionState = DownloadStatusTrackerState;

    fn new(param: Self::ExtensionParam, _downloader: &mut HttpFileDownloader) -> (Self, Self::ExtensionState) where Self: Sized {
        let (status_sender, status_receiver) = sync::watch::channel(DownloaderStatus::NoStart);
        let status_sender = Arc::new(DownloadStatusSender {
            log: param.log,
            status_sender,
        });
        (
            DownloadStatusTrackerController {
                status_receiver: status_receiver.clone(),
                status_sender: status_sender.clone(),
            },
            DownloadStatusTrackerState {
                status_receiver,
                status_sender,
            },
        )
    }
}

#[async_trait]
impl DownloaderWrapper for DownloadStatusTrackerController {
    async fn prepare_download(&mut self, _downloader: &mut HttpFileDownloader) -> Result<(), DownloadStartError> {
        match self.status() {
            DownloaderStatus::Running => return Err(DownloadStartError::AlreadyDownloading),
            DownloaderStatus::Pending(pending_type) => match pending_type {
                NetworkItemPendingType::Starting => {
                    return Err(DownloadStartError::Starting);
                }
                NetworkItemPendingType::Stopping => {
                    return Err(DownloadStartError::Stopping);
                }
                NetworkItemPendingType::Initializing => {
                    return Err(DownloadStartError::Initializing);
                }
                _ => {}
            },
            _ => {}
        };
        self.status_sender.change_status(DownloaderStatus::Pending(NetworkItemPendingType::Starting));
        Ok(())
    }

    async fn handle_prepare_download(&mut self, _downloader: &mut HttpFileDownloader, prepare_download_result: Result<DownloadFuture, DownloadStartError>) -> Result<DownloadFuture, DownloadStartError> {
        match prepare_download_result {
            Ok(download_future) => Ok(download_future),
            Err(err) => {
                self.status_sender.change_status(DownloaderStatus::Error(err.to_string()));
                Err(err)
            }
        }
    }

    async fn download(
        &mut self,
        downloader: &mut HttpFileDownloader,
        mut download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError> {
        self.status_sender.change_status(DownloaderStatus::Pending(
            NetworkItemPendingType::Initializing,
        ));
        let (download_way_sender, download_way_receiver) = sync::oneshot::channel();

        downloader
            .downloading_state_oneshot_vec
            .push(download_way_sender);
        let status_sender = self.status_sender.clone();
        Ok(async move {
            select! {
                _ = download_way_receiver => {
                    status_sender.change_status(DownloaderStatus::Running);
                },
                r = (&mut download_future) =>{
                    match &r {
                        Ok(end_cause) => match end_cause {
                            DownloadingEndCause::DownloadFinished => {
                                status_sender.change_status(DownloaderStatus::Finished)
                            }
                            DownloadingEndCause::Cancelled => {
                                status_sender.change_status(DownloaderStatus::NoStart)
                            }
                        },
                        Err(err) => status_sender.change_status(DownloaderStatus::Error(err.to_string())),
                    };
                    return r;
                }
            }
            let r = download_future.await;
            match &r {
                Ok(end_cause) => match end_cause {
                    DownloadingEndCause::DownloadFinished => {
                        status_sender.change_status(DownloaderStatus::Finished)
                    }
                    DownloadingEndCause::Cancelled => {
                        status_sender.change_status(DownloaderStatus::NoStart)
                    }
                },
                Err(err) => status_sender.change_status(DownloaderStatus::Error(err.to_string())),
            };
            r
        }
            .boxed())
    }

    async fn cancel(&self, cancel_future: DownloadCancelFuture<'_>) -> Result<(), DownloadStopError> {
        self.status_sender
            .change_status(DownloaderStatus::Pending(NetworkItemPendingType::Stopping));
        cancel_future.await
    }
}

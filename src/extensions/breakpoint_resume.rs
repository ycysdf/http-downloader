use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::{select, sync};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{ChunkInfo, ChunkManager, ChunkRange, DownloadArchiveData, DownloadController, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, DownloadWay, HttpDownloadConfig, HttpFileDownloader};

pub enum FileSave {
    Absolute(PathBuf),
    Suffix(String),
}

impl FileSave {
    pub fn get_file_path(&self, origin_file: &Path) -> Cow<PathBuf> {
        match self {
            FileSave::Absolute(path) => { Cow::Borrowed(path) }
            FileSave::Suffix(suffix) => { Cow::Owned(origin_file.with_extension(OsStr::new(&format!("{}.{}", origin_file.extension().and_then(|n| n.to_str()).unwrap_or(""), suffix)))) }
        }
    }
}

pub struct DownloadBreakpointResumeExtension<T: DownloadDataArchiverBuilder> {
    pub download_archiver_builder: T,
}

#[async_trait]
pub trait DownloadDataArchiver: Send + Sync + 'static {
    async fn save(&self, data: Box<DownloadArchiveData>) -> Result<(), anyhow::Error>;
    async fn load(&self) -> Result<Option<Box<DownloadArchiveData>>, anyhow::Error>;
    async fn download_started(&self, download_way: &Arc<DownloadWay>, is_resume: bool)->Result<(),anyhow::Error>;
    async fn download_finished(&self);
}

pub trait DownloadDataArchiverBuilder {
    type DownloadDataArchiver: DownloadDataArchiver;
    fn build(self, config: &HttpDownloadConfig) -> Self::DownloadDataArchiver;
}

impl<DC: DownloadController, T: DownloadDataArchiverBuilder> DownloadExtension<DC> for DownloadBreakpointResumeExtension<T> {
    type DownloadController = DownloadBreakpointResumeController<DC, T::DownloadDataArchiver>;
    type ExtensionState = DownloadBreakpointResumeState;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        let DownloadBreakpointResumeExtension { download_archiver_builder } = self;

        let download_archiver = download_archiver_builder.build(&downloader.config);
        (
            Arc::new(DownloadBreakpointResumeController {
                inner,
                // archive_data_sender: sender,
                download_archiver: Arc::new(download_archiver),
            }),
            DownloadBreakpointResumeState {},
        )
    }
}

pub struct DownloadBreakpointResumeState {}

pub struct DownloadBreakpointResumeController<DC: DownloadController, T: DownloadDataArchiver> {
    inner: Arc<DC>,
    pub download_archiver: Arc<T>,
}

impl<DC: DownloadController, T: DownloadDataArchiver> DownloadBreakpointResumeController<DC, T> {
    async fn save_archive_data(&self, chunk_manager_mutex: Arc<Mutex<Option<Arc<ChunkManager>>>>) -> Result<(), anyhow::Error> {
        let archive_data = {
            let chunk_manager = chunk_manager_mutex.lock().await;
            if let Some(chunk_manager) = chunk_manager.as_ref() {
                let mut data = chunk_manager.chunk_iterator.data.lock().clone();
                data.last_incomplete_chunks.extend(chunk_manager.get_chunks().await.iter().filter_map(|n| {
                    let downloaded_len = n.downloaded_len.load(Ordering::SeqCst);
                    if downloaded_len == n.chunk_info.range.len() {
                        None
                    } else {
                        let start = n.chunk_info.range.start + downloaded_len;
                        let end = n.chunk_info.range.end;
                        Some(ChunkInfo {
                            index: n.chunk_info.index,
                            range: ChunkRange::new(start, end),
                        })
                    }
                }));
                Some(DownloadArchiveData {
                    downloaded_len: chunk_manager.chunk_iterator.content_length - data.remaining_len(),
                    downloading_duration: chunk_manager.downloading_duration.load(Ordering::Relaxed),
                    chunk_data: Some(data),
                })
            } else {
                None
            }
        };
        if let Some(archive_data) = archive_data {
            self.download_archiver.save(Box::new(archive_data)).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<DC: DownloadController, T: DownloadDataArchiver> DownloadController for DownloadBreakpointResumeController<DC, T> {
    async fn download(
        self: Arc<Self>,
        mut params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        let (sender, receiver) = sync::oneshot::channel();
        params.breakpoint_resume = true;
        params.archive_data = self.download_archiver.load().await?;
        let is_resume = !params.archive_data.is_none();

        params.download_way_oneshot_vec.push(sender);
        let download_future = self.inner.to_owned().download(params).await?;

        let chunk_manager_mutex = Arc::new(tokio::sync::Mutex::new(None));
        let future = {
            let download_archiver = self.download_archiver.clone();
            let chunk_manager_mutex = chunk_manager_mutex.clone();
            async move {
                let download_way_receiver = receiver.await.map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;
                download_archiver.download_started(&download_way_receiver,is_resume).await?;
                if let DownloadWay::Ranges(chunk_manager) = download_way_receiver.as_ref() {
                    { *chunk_manager_mutex.lock().await = Some(chunk_manager.clone()); }
                    let mut notified =
                        chunk_manager.data_archive_notify.notified();

                    loop {
                        notified.await;

                        let mut data = chunk_manager.chunk_iterator.data.lock().clone();
                        data.last_incomplete_chunks.extend(chunk_manager.get_chunks().await.iter().map(|n| n.chunk_info.to_owned()));
                        let archive_data = DownloadArchiveData {
                            downloaded_len: chunk_manager.chunk_iterator.content_length - data.remaining_len(),
                            downloading_duration: chunk_manager.downloading_duration.load(Ordering::Relaxed),
                            chunk_data: Some(data),
                        };
                        notified = chunk_manager.data_archive_notify.notified();
                        chunk_manager.archive_complete_notify.notify_one();
                        download_archiver.save(Box::new(archive_data)).await?;
                    }
                } else {
                    let download_finished_token = CancellationToken::new();
                    download_finished_token.cancelled().await;
                    unreachable!();
                }
            }
        };

        #[allow(clippy::collapsible_match)]
        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    if let Ok(r) = r{
                        match r {
                            DownloadingEndCause::DownloadFinished => {
                                self.download_archiver.download_finished().await;
                            }
                            DownloadingEndCause::Cancelled => {
                                self.save_archive_data(chunk_manager_mutex).await
                                    .unwrap_or_else(|err| {
                                        #[cfg(feature = "tracing")]
                                        tracing::warn!("save_archive_data failed! {:?}",err);
                                    });
                            }
                        }
                    }
                    r
                }
            }
        }.boxed())
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.inner.cancel().await
    }
}

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::FutureExt;
use tokio::{select, sync};

use crate::{BreakpointResume, ChunkInfo, ChunkRange, DownloadArchiveData, DownloadError, DownloaderWrapper, DownloadExtensionBuilder, DownloadFuture, DownloadingEndCause, DownloadingState, DownloadStartError, DownloadWay, HttpDownloadConfig, HttpFileDownloader};

pub enum FileSave {
    Absolute(PathBuf),
    Suffix(String),
}

impl FileSave {
    pub fn get_file_path(&self, origin_file: &Path) -> Cow<PathBuf> {
        match self {
            FileSave::Absolute(path) => Cow::Borrowed(path),
            FileSave::Suffix(suffix) => Cow::Owned(
                origin_file.with_extension(OsStr::new(&format!(
                    "{}.{}",
                    origin_file
                        .extension()
                        .and_then(|n| n.to_str())
                        .unwrap_or(""),
                    suffix
                ))),
            ),
        }
    }
}

pub struct DownloadBreakpointResumeExtension<T: DownloadDataArchiverBuilder> {
    pub download_archiver_builder: T,
}

pub enum DownloadEndInfo<'a> {
    StartError(&'a DownloadStartError),
    DownloadEnd(&'a Result<DownloadingEndCause, DownloadError>),
}

#[async_trait]
pub trait DownloadDataArchiver: Send + Sync + 'static {
    async fn save(&self, data: Box<DownloadArchiveData>) -> Result<(), anyhow::Error>;
    async fn load(&self) -> Result<Option<Box<DownloadArchiveData>>, anyhow::Error>;
    async fn download_started(
        &self,
        download_way: &Arc<DownloadingState>,
        is_resume: bool,
    ) -> Result<(), anyhow::Error>;
    async fn download_ended<'a>(&'a self, end_info: DownloadEndInfo<'a>);
}

pub trait DownloadDataArchiverBuilder {
    type DownloadDataArchiver: DownloadDataArchiver;
    fn build(self, config: &HttpDownloadConfig) -> Self::DownloadDataArchiver;
}

pub struct DownloadBreakpointResumeState<T: DownloadDataArchiverBuilder> {
    pub download_archiver: Arc<T::DownloadDataArchiver>,
}

pub struct DownloadBreakpointResumeDownloaderWrapper<T: DownloadDataArchiverBuilder> {
    pub download_archiver: Arc<T::DownloadDataArchiver>,
    breakpoint_resume: Option<Arc<BreakpointResume>>,
    pub receiver: Option<sync::oneshot::Receiver<Arc<DownloadingState>>>
}

impl<T: DownloadDataArchiverBuilder+'static> DownloadExtensionBuilder for DownloadBreakpointResumeExtension<T> {
    type Wrapper = DownloadBreakpointResumeDownloaderWrapper<T>;
    type ExtensionState = DownloadBreakpointResumeState<T>;

    fn build(self, downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) where Self: Sized {
        let DownloadBreakpointResumeExtension {
            download_archiver_builder,
        } = self;

        let download_archiver = Arc::new(download_archiver_builder.build(&downloader.config));
        (
            DownloadBreakpointResumeDownloaderWrapper {
                download_archiver: download_archiver.clone(),
                breakpoint_resume: None,
                receiver: None,
            },
            DownloadBreakpointResumeState {
                download_archiver,
            },
        )
    }
}

#[async_trait]
impl<T: DownloadDataArchiverBuilder+'static> DownloaderWrapper for DownloadBreakpointResumeDownloaderWrapper<T>
{
    async fn prepare_download(&mut self, downloader: &mut HttpFileDownloader) -> Result<(), DownloadStartError> {
        let (sender, receiver) = sync::oneshot::channel();

        downloader.breakpoint_resume = Some(Arc::new(BreakpointResume::default()));
        downloader.archive_data = self.download_archiver.load().await?;
        downloader.downloading_state_oneshot_vec.push(sender);
        self.breakpoint_resume = downloader.breakpoint_resume.clone();
        self.receiver = Some(receiver);
        Ok(())
    }

    async fn handle_prepare_download_result(&mut self, _downloader: &mut HttpFileDownloader, prepare_download_result: Result<DownloadFuture, DownloadStartError>) -> Result<DownloadFuture, DownloadStartError> {
        match prepare_download_result {
            Ok(download_future) => Ok(download_future),
            Err(err) => {
                self.download_archiver
                    .download_ended(DownloadEndInfo::StartError(&err))
                    .await;
                return Err(err);
            }
        }
    }

    async fn download(
        &mut self,
        downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError>
    {
        let notifies = self.breakpoint_resume.as_ref().unwrap().clone();
        let receiver= self.receiver.take().unwrap();

        let is_resume= downloader.archive_data.is_some();

        let future = {
            let download_archiver = self.download_archiver.clone();
            async move {
                let downloading_state = receiver
                    .await
                    .map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;
                download_archiver
                    .download_started(&downloading_state, is_resume)
                    .await?;
                if let DownloadWay::Ranges(chunk_manager) = &downloading_state.download_way {
                    let mut notified = notifies.data_archive_notify.notified();

                    loop {
                        notified.await;

                        let mut data = {
                            let data = chunk_manager.chunk_iterator.data.read();
                            data.clone()
                        };
                        data.last_incomplete_chunks.extend(
                            chunk_manager.get_chunks().await.iter().filter_map(|n| {
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
                            }),
                        );
                        let archive_data = DownloadArchiveData {
                            downloaded_len: chunk_manager.chunk_iterator.content_length
                                - data.remaining_len(),
                            downloading_duration: downloading_state.get_current_downloading_duration(),
                            chunk_data: Some(data),
                        };
                        notified = notifies.data_archive_notify.notified();
                        notifies.archive_complete_notify.notify_one();
                        download_archiver.save(Box::new(archive_data)).await?;
                    }
                } else {
                    futures_util::future::pending().await
                }
            }
        };

        let download_archiver = self.download_archiver.clone();
        #[allow(clippy::collapsible_match)]
        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    download_archiver.download_ended(DownloadEndInfo::DownloadEnd(&r)).await;
                    r
                }
            }
        }
            .boxed())
    }
}

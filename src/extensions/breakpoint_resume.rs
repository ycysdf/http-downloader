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

use crate::{ChunkInfo, ChunkRange, DownloadArchiveData, DownloadController, DownloadError, DownloadExtensionOld, DownloadingEndCause, DownloadingState, DownloadContext, DownloadStartError, DownloadStopError, DownloadWay, HttpDownloadConfig, HttpFileDownloader};

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

impl<DC: DownloadController, T: DownloadDataArchiverBuilder> DownloadExtensionOld<DC>
for DownloadBreakpointResumeExtension<T>
{
    type DownloadController = DownloadBreakpointResumeController<DC, T::DownloadDataArchiver>;
    type ExtensionState = DownloadBreakpointResumeState<T::DownloadDataArchiver>;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        let DownloadBreakpointResumeExtension {
            download_archiver_builder,
        } = self;

        let download_archiver = download_archiver_builder.build(&downloader.config);
        let download_archiver = Arc::new(download_archiver);
        (
            Arc::new(DownloadBreakpointResumeController {
                inner,
                // archive_data_sender: sender,
                download_archiver: download_archiver.clone(),
            }),
            DownloadBreakpointResumeState {
                download_archiver,
            },
        )
    }
}

pub struct DownloadBreakpointResumeState<T: DownloadDataArchiver> {
    pub download_archiver: Arc<T>,
}

pub struct DownloadBreakpointResumeController<DC: DownloadController, T: DownloadDataArchiver> {
    inner: Arc<DC>,
    pub download_archiver: Arc<T>,
}

#[async_trait]
impl<DC: DownloadController, T: DownloadDataArchiver> DownloadController
for DownloadBreakpointResumeController<DC, T>
{
    async fn download(
        self: Arc<Self>,
        mut params: DownloadContext,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError>
    {
        let (sender, receiver) = sync::oneshot::channel();
        params.breakpoint_resume = true;
        params.archive_data = self.download_archiver.load().await?;
        let is_resume = params.archive_data.is_some();

        params.downloading_state_oneshot_vec.push(sender);
        let download_future = match self.inner.to_owned().download(params).await {
            Ok(r) => r,
            Err(err) => {
                self.download_archiver
                    .download_ended(DownloadEndInfo::StartError(&err))
                    .await;
                return Err(err);
            }
        };

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
                    let mut notified = chunk_manager.data_archive_notify.notified();

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
                        notified = chunk_manager.data_archive_notify.notified();
                        chunk_manager.archive_complete_notify.notify_one();
                        download_archiver.save(Box::new(archive_data)).await?;
                    }
                } else {
                    futures_util::future::pending().await
                }
            }
        };

        #[allow(clippy::collapsible_match)]
        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    self.download_archiver.download_ended(DownloadEndInfo::DownloadEnd(&r)).await;
                    r
                }
            }
        }
            .boxed())
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.inner.cancel().await
    }
}

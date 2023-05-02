use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc};
use std::sync::atomic::Ordering;

use anyhow::Result;
use futures_util::future::{BoxFuture};
use futures_util::FutureExt;
use tokio::{select, sync};

use crate::{BreakpointResume, ChunkInfo, ChunkRange, DownloadArchiveData, DownloaderWrapper, DownloadExtensionBuilder, DownloadFuture, DownloadingState, DownloadStartError, DownloadWay, HttpDownloadConfig, HttpFileDownloader};
use crate::exclusive::Exclusive;

pub enum FileSave {
    AbsolutePath(PathBuf),
    OriginPathWithSuffix(String),
}

impl FileSave {
    pub fn get_file_path(&self, origin_file: &Path) -> Cow<PathBuf> {
        match self {
            FileSave::AbsolutePath(path) => Cow::Borrowed(path),
            FileSave::OriginPathWithSuffix(suffix) => Cow::Owned(
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

impl<T: DownloadDataArchiverBuilder> DownloadBreakpointResumeExtension<T> {
    pub fn new(download_archiver_builder:T)->Self{
        Self{
            download_archiver_builder
        }
    }
}

pub trait DownloadDataArchiver: Send + Sync + 'static {
    fn save(&self, data: Box<DownloadArchiveData>) -> BoxFuture<'static,Result<()>>;
    fn load(&self) -> BoxFuture<'static,Result<Option<Box<DownloadArchiveData>>>>;
    fn clear(&self){}
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
    pub receiver: Option<sync::oneshot::Receiver<Arc<DownloadingState>>>,
}

impl<T: DownloadDataArchiverBuilder + 'static> DownloadExtensionBuilder for DownloadBreakpointResumeExtension<T> {
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

impl<T: DownloadDataArchiverBuilder + 'static> DownloaderWrapper for DownloadBreakpointResumeDownloaderWrapper<T>
{
    fn prepare_download(&mut self, downloader: &mut HttpFileDownloader) -> Result<(), DownloadStartError> {
        let (sender, receiver) = sync::oneshot::channel();

        downloader.breakpoint_resume = Some(Arc::new(BreakpointResume::default()));
        downloader.archive_data_future = Some(Exclusive::new(self.download_archiver.load()));
        downloader.downloading_state_oneshot_vec.push(sender);
        self.breakpoint_resume = downloader.breakpoint_resume.clone();
        self.receiver = Some(receiver);
        Ok(())
    }

    fn download(
        &mut self,
        downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError>
    {
        let notifies = self.breakpoint_resume.as_ref().unwrap().clone();
        let receiver = self.receiver.take().unwrap();
        let cancel_token = downloader.cancel_token.clone();

        // let is_resume = downloader.archive_data_future.is_some();

        let future = {
            let download_archiver = self.download_archiver.clone();
            async move {
                let downloading_state = receiver
                    .await
                    .map_err(|_| anyhow::Error::msg("ReceiveDownloadWawFailed"))?;
                if let DownloadWay::Ranges(chunk_manager) = &downloading_state.download_way {
                    let mut notified = notifies.data_archive_notify.notified();

                    loop {
                        notified.await;

                        let mut data = {
                            let data = chunk_manager.chunk_iterator.data.read();
                            data.clone()
                        };
                        let downloading_chunks = chunk_manager.get_chunks().await;
                        if cancel_token.is_cancelled(){
                            data.last_incomplete_chunks.extend(
                                downloading_chunks.iter().filter_map(|n| {
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
                        }else{
                            data.remaining.ranges.extend(downloading_chunks.into_iter().map(|n|n.chunk_info.range));
                            data.remaining.ranges.sort_by_key(|n|n.start);
                        }
                        let archive_data = DownloadArchiveData {
                            downloaded_len: chunk_manager.chunk_iterator.content_length
                                - data.remaining_len(),
                            downloading_duration: downloading_state.get_current_downloading_duration(),
                            chunk_data: Some(data),
                        };
                        download_archiver.save(Box::new(archive_data)).await?;
                        notified = notifies.data_archive_notify.notified();
                        notifies.archive_complete_notify.notify_one();
                    }
                } else {
                    futures_util::future::pending().await
                }
            }
        };

        // let download_archiver = self.download_archiver.clone();
        #[allow(clippy::collapsible_match)]
        Ok(async move {
            select! {
                r = future => {r},
                r = download_future => {
                    r
                }
            }
        }
            .boxed())
    }
}

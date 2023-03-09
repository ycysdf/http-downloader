use std::future::Future;
use std::io::SeekFrom;
use std::num::{NonZeroU64, NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use futures_util::{FutureExt, TryFutureExt};
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use headers::HeaderMapExt;
use parking_lot::RwLock;
use thiserror::Error;
use tokio::{io, sync};
use tokio::io::AsyncSeekExt;
use tokio::sync::watch::error::SendError;
use tokio::task::JoinError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "tracing")]
use tracing::Instrument;

use crate::{ChunkData, ChunkItem, ChunkIterator, ChunkManager, ChunksInfo, DownloadArchiveData, DownloadController, DownloadedLenChangeNotify, DownloaderWrapper, DownloadFuture, DownloadWay, HttpDownloadConfig, RemainingChunks, SingleDownload};
#[cfg(feature = "status-tracker")]
use crate::status_tracker::DownloaderStatus;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum DownloadingEndCause {
    DownloadFinished,
    Cancelled,
}

#[derive(Error, Debug)]
pub enum DownloadStartError {
    #[error("file create failed，{:?}", .0)]
    FileCrateFailed(#[from] io::Error),
    #[error("{:?}", .0)]
    Other(#[from] anyhow::Error),

    #[error("already downloading")]
    AlreadyDownloading,
    #[error("Directory does not exist")]
    DirectoryDoesNotExist,

    #[cfg(feature = "status-tracker")]
    #[error("Initializing")]
    Initializing,
    #[cfg(feature = "status-tracker")]
    #[error("Starting")]
    Starting,
    #[cfg(feature = "status-tracker")]
    #[error("Stopping")]
    Stopping,
}

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("{:?}", .0)]
    Other(#[from] anyhow::Error),
    #[error("IoError，{:?}", .0)]
    IoError(#[from] io::Error),
    #[error("ContentLengthInvalid")]
    ContentLengthInvalid,
    #[error("JoinError，{:?}", .0)]
    JoinError(#[from] JoinError),
    #[error("chunk remove failed，{:?}", .0)]
    ChunkRemoveFailed(usize),
    #[error("downloading chunk remove failed，{:?}", .0)]
    DownloadingChunkRemoveFailed(usize),
    #[error("http request failed，{:?}", .0)]
    HttpRequestFailed(#[from] reqwest::Error),
    #[error("server file already changed")]
    ServerFileAlreadyChanged,
}

#[derive(Error, Debug)]
pub enum DownloadToEndError {
    #[error("{:?}", .0)]
    DownloadStartError(#[from] DownloadStartError),
    #[error("{:?}", .0)]
    DownloadError(#[from] DownloadError),
}

#[derive(Error, Debug)]
pub enum DownloadStopError {
    #[error("recv error")]
    RecvError(#[from] sync::oneshot::error::RecvError),
    #[error("download already finished")]
    DownloadAlreadyFinished,
    #[error("http request failed")]
    RemoveFileError(#[from] io::Error),
    #[error("it is no start")]
    NoStart,
    #[cfg(feature = "status-tracker")]
    #[error("downloader status send error")]
    SendError(#[from] SendError<DownloaderStatus>),
}

#[derive(Error, Debug)]
pub enum ChangeConnectionCountError {
    #[error("SendError")]
    SendError(#[from] SendError<NonZeroU8>),
    #[error("it is no start")]
    NoStart,
    #[error("The download target is not supported")]
    DownloadTargetNotSupported,
}

#[derive(Error, Debug)]
pub enum ChangeChunkSizeError {
    #[error("it is no start")]
    NoStart,
    #[error("The download target is not supported")]
    DownloadTargetNotSupported,
}

pub struct DownloadingState {
    pub downloading_duration: u32,
    pub download_instant: Instant,
    pub download_way: DownloadWay,
}

impl DownloadingState {
    pub fn get_current_downloading_duration(&self) -> u32 {
        self.downloading_duration + self.download_instant.elapsed().as_secs() as u32
    }
}

pub struct HttpFileDownloader {
    pub downloading_state_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadingState>>>,
    pub downloaded_len_change_notify: Option<Arc<dyn DownloadedLenChangeNotify>>,
    pub archive_data: Option<Box<DownloadArchiveData>>,
    pub config: Arc<HttpDownloadConfig>,
    pub downloaded_len_receiver: sync::watch::Receiver<u64>,
    content_length: Arc<AtomicU64>,
    client: reqwest::Client,
    downloading_state: Arc<RwLock<
        Option<(
            sync::oneshot::Receiver<DownloadingEndCause>,
            Arc<DownloadingState>,
        )>,
    >>,
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    pub cancel_token: CancellationToken,
    total_size_semaphore: Arc<sync::Semaphore>,
}

impl HttpFileDownloader {
    pub fn new(client: reqwest::Client, config: Arc<HttpDownloadConfig>) -> Self {
        let cancel_token = CancellationToken::new();
        let (downloaded_len_sender, downloaded_len_receiver) = sync::watch::channel::<u64>(0);
        let total_size_semaphore = Arc::new(sync::Semaphore::new(0));

        Self {
            downloading_state_oneshot_vec: vec![],
            downloaded_len_change_notify: None,
            archive_data: None,
            config,
            total_size_semaphore,
            content_length: Default::default(),
            client,
            downloading_state: Default::default(),
            downloaded_len_receiver,
            downloaded_len_sender: Arc::new(downloaded_len_sender),
            cancel_token,
        }
    }

    pub fn is_downloading(&self) -> bool {
        self.downloading_state.read().is_some()
    }

    pub fn change_connection_count(
        &self,
        connection_count: NonZeroU8,
    ) -> Result<(), ChangeConnectionCountError> {
        match self.downloading_state.read().as_ref() {
            None => Err(ChangeConnectionCountError::NoStart),
            Some((_, downloading_state)) => match &downloading_state.download_way {
                DownloadWay::Single(_) => {
                    Err(ChangeConnectionCountError::DownloadTargetNotSupported)
                }
                DownloadWay::Ranges(chunk_manager) => {
                    chunk_manager.change_connection_count(connection_count)?;
                    Ok(())
                }
            },
        }
    }
    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) -> Result<(), ChangeChunkSizeError> {
        match self.downloading_state.read().as_ref() {
            None => Err(ChangeChunkSizeError::NoStart),
            Some((_, downloading_state)) => match &downloading_state.download_way {
                DownloadWay::Single(_) => Err(ChangeChunkSizeError::DownloadTargetNotSupported),
                DownloadWay::Ranges(chunk_manager) => {
                    chunk_manager.change_chunk_size(chunk_size);
                    Ok(())
                }
            },
        }
    }

    #[cfg(feature = "async-stream")]
    pub async fn downloaded_len_stream(&self) -> impl Stream<Item=u64> {
        let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
        let duration = self.config.downloaded_len_send_interval.clone();
        async_stream::stream! {
            let downloaded_len = *downloaded_len_receiver.borrow();
            yield downloaded_len;
            while downloaded_len_receiver.changed().await.is_ok() {
                let downloaded_len = *downloaded_len_receiver.borrow();
                yield downloaded_len;
                if let Some(duration) = duration{
                   tokio::time::sleep(duration).await;
                }
            }
        }
    }

    #[cfg(feature = "async-stream")]
    pub async fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>>> {
        match self.downloading_state.read().as_ref() {
            None => {
                // tracing::info!("downloading_state is null!");
                None
            }
            Some((_, downloading_state)) => match &downloading_state.download_way {
                DownloadWay::Single(_) => {
                    // tracing::info!("DownloadWay is Single!");
                    None
                }
                DownloadWay::Ranges(chunk_manager) => {
                    let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
                    let chunk_manager = chunk_manager.to_owned();
                    let duration = self.config.chunks_send_interval.clone();
                    Some(async_stream::stream! {
                          yield chunk_manager.get_chunks().await;
                          while downloaded_len_receiver.changed().await.is_ok() {
                              yield chunk_manager.get_chunks().await;
                              if let Some(duration) = duration {
                                 tokio::time::sleep(duration).await;
                              }
                          }
                    })
                }
            },
        }
    }

    #[cfg(feature = "async-stream")]
    pub async fn chunks_info_stream(&self) -> Option<impl Stream<Item=ChunksInfo>> {
        match self.downloading_state.read().as_ref() {
            None => {
                // tracing::info!("downloading_state is null!");
                None
            }
            Some((_, downloading_state)) => match &downloading_state.download_way {
                DownloadWay::Single(_) => {
                    // tracing::info!("DownloadWay is Single!");
                    None
                }
                DownloadWay::Ranges(chunk_manager) => {
                    let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
                    let chunk_manager = chunk_manager.to_owned();
                    let duration = self.config.chunks_send_interval.clone();
                    Some(async_stream::stream! {
                          yield chunk_manager.get_chunks_info().await;
                          while downloaded_len_receiver.changed().await.is_ok() {
                              yield chunk_manager.get_chunks_info().await;
                              if let Some(duration) = duration {
                                 tokio::time::sleep(duration).await;
                              }
                          }
                    })
                }
            },
        }
    }

    pub fn downloaded_len(&self) -> u64 {
        *self.downloaded_len_receiver.borrow()
    }

    pub async fn total_size(&self) -> Option<NonZeroU64> {
        let _ = self.total_size_semaphore.acquire().await;
        let content_length = self.content_length.load(Ordering::Relaxed);
        if content_length == 0 {
            None
        } else {
            Some(NonZeroU64::new(content_length).unwrap())
        }
    }

    pub fn current_total_size(&self) -> Option<NonZeroU64> {
        let content_length = self.content_length.load(Ordering::Relaxed);
        if content_length == 0 {
            None
        } else {
            Some(NonZeroU64::new(content_length).unwrap())
        }
    }

    pub fn get_chunk_manager(&self) -> Option<Arc<ChunkManager>> {
        self.get_downloading_state().and_then(|downloading_state| {
            if let DownloadWay::Ranges(item) = &downloading_state.download_way {
                Some(item.clone())
            } else {
                None
            }
        })
    }
    pub fn get_downloading_state(&self) -> Option<Arc<DownloadingState>> {
        let guard = self.downloading_state.read();
        guard.as_ref().map(|n| n.1.to_owned())
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        match self.get_chunk_manager() {
            None => Vec::new(),
            Some(n) => n.get_chunks().await,
        }
    }

    pub async fn get_chunks_info(&self) -> Option<ChunksInfo> {
        match self.get_chunk_manager() {
            None => None,
            Some(n) => Some(n.get_chunks_info().await),
        }
    }

    pub fn get_file_path(&self) -> PathBuf {
        self.config.file_path()
    }

    fn reset(&self) {
        self.downloaded_len_sender.send(0).unwrap_or_else(|_err| {
            #[cfg(feature = "tracing")]
            tracing::trace!("send downloaded_len failed! {}", _err);
        });
    }

    pub(crate) async fn download(
        &mut self,
    ) -> Result<
        impl Future<Output=Result<DownloadingEndCause, DownloadError>> + 'static,
        DownloadStartError,
    > {
        self.reset();
        if self.is_downloading() {
            return Err(DownloadStartError::AlreadyDownloading);
        }

        if self.config.create_dir {
            std::fs::create_dir_all(&self.config.save_dir)?;
        } else if !self.config.save_dir.exists() {
            return Err(DownloadStartError::DirectoryDoesNotExist);
        }
        Ok(self.start_download())
    }

    pub fn take_downloading_state(
        &self,
    ) -> Option<(
        sync::oneshot::Receiver<DownloadingEndCause>,
        Arc<DownloadingState>,
    )> {
        let mut guard = self.downloading_state.write();
        guard.take()
    }

    pub async fn cancel(&self) -> Result<(), DownloadStopError> {
        {
            self.cancel_token.cancel();
        }
        if let Some((receiver, _downloading_state)) = self.take_downloading_state() {
            match receiver.await? {
                DownloadingEndCause::DownloadFinished => {
                    Err(DownloadStopError::DownloadAlreadyFinished)
                }
                DownloadingEndCause::Cancelled => Ok(()),
            }
        } else {
            Err(DownloadStopError::NoStart)
        }
    }

    //noinspection RsExternalLinter
    fn start_download(
        &mut self,
    ) -> impl Future<Output=Result<DownloadingEndCause, DownloadError>> + 'static {
        if self.cancel_token.is_cancelled() {
            self.cancel_token = CancellationToken::new();
        }
        let config = self.config.clone();
        let client = self.client.clone();
        let total_size_semaphore = self.total_size_semaphore.clone();
        let content_length_arc = self.content_length.clone();
        let downloading_state = self.downloading_state.clone();
        let downloaded_len_change_notify = self.downloaded_len_change_notify.take();
        let archive_data = self.archive_data.take();
        let downloading_state_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadingState>>> = self.downloading_state_oneshot_vec.drain(..).collect();
        let downloaded_len_sender = self.downloaded_len_sender.clone();
        let cancel_token = self.cancel_token.clone();

        async move {
            let response = client.execute(config.create_http_request());
            #[cfg(feature = "tracing")]
                let response = response.instrument(tracing::info_span!("request for content_length"));

            let response = match response.await {
                Ok(response) => response,
                Err(err) => {
                    total_size_semaphore.add_permits(1);
                    return Err(err.into());
                }
            };
            let response = match response.error_for_status() {
                Ok(response) => response,
                Err(err) => {
                    total_size_semaphore.add_permits(1);
                    return Err(err.into());
                }
            };
            let etag = {
                if config.etag.is_some() {
                    let cur_etag = response.headers().typed_get::<headers::ETag>();
                    if cur_etag == config.etag {
                        #[cfg(feature = "tracing")]
                        tracing::trace!(
                        "etag mismatching,your etag: {:?} , current etag:{:?}",
                        config.etag,
                        cur_etag
                    );
                        total_size_semaphore.add_permits(1);
                        return Err(DownloadError::ServerFileAlreadyChanged);
                    }
                    cur_etag
                } else {
                    None
                }
            };
            let mut content_length = response
                .headers()
                .typed_get::<headers::ContentLength>()
                .map(|n| n.0);
            if config.handle_zero_content {
                content_length = content_length.and_then(|n| if n == 0 { None } else { Some(n) });
            }
            let accept_ranges = response.headers().typed_get::<headers::AcceptRanges>();

            if let Some(content_length) = content_length {
                if content_length == 0 {
                    total_size_semaphore.add_permits(1);
                    return Err(DownloadError::ContentLengthInvalid);
                }
            }
            content_length_arc.store(content_length.unwrap_or(0), Ordering::Relaxed);
            total_size_semaphore.add_permits(1);

            let mut options = std::fs::OpenOptions::new();
            (config.open_option)(&mut options);
            let mut file = tokio::fs::OpenOptions::from(options)
                .open(config.file_path())
                .await?;
            if config.set_len_in_advance {
                file.set_len(content_length.unwrap()).await?
            }
            file.seek(SeekFrom::Start(0)).await?;

            let is_ranges_bytes_none = accept_ranges.is_none();
            let is_ranges_bytes =
                !is_ranges_bytes_none && accept_ranges.unwrap() == headers::AcceptRanges::bytes();
            let downloading_duration = archive_data.as_ref()
                .map(|n| n.downloading_duration)
                .unwrap_or(0);
            let download_way = {
                if content_length.is_some()
                    && (if config.strict_check_accept_ranges {
                    is_ranges_bytes
                } else {
                    is_ranges_bytes_none || is_ranges_bytes
                })
                {
                    let content_length = content_length.unwrap();
                    let chunk_data = archive_data
                        .and_then(|archive_data| {
                            downloaded_len_sender
                                .send(archive_data.downloaded_len)
                                .unwrap_or_else(|_err| {
                                    #[cfg(feature = "tracing")]
                                    tracing::error!("send downloaded_len failed! {}", _err);
                                });
                            archive_data.chunk_data.map(|mut data| {
                                data.remaining.chunk_size = config.chunk_size.get();
                                data
                            })
                        })
                        .unwrap_or_else(|| ChunkData {
                            iter_count: 0,
                            remaining: RemainingChunks::new(config.chunk_size, content_length),
                            last_incomplete_chunks: Default::default(),
                        });

                    let chunk_iterator = ChunkIterator::new(content_length, chunk_data);
                    let chunk_manager = Arc::new(ChunkManager::new(
                        config.download_connection_count,
                        client,
                        cancel_token,
                        downloaded_len_sender,
                        chunk_iterator,
                        etag,
                        config.request_retry_count,
                    ));
                    DownloadWay::Ranges(chunk_manager)
                } else {
                    DownloadWay::Single(SingleDownload::new(
                        cancel_token,
                        downloaded_len_sender,
                        content_length,
                    ))
                }
            };

            let state = DownloadingState {
                downloading_duration,
                download_instant: Instant::now(),
                download_way,
            };

            let (end_sender, end_receiver) = sync::oneshot::channel();

            let state = Arc::new(state);
            {
                let mut guard = downloading_state.write();
                *guard = Some((end_receiver, state.clone()));
            }

            for oneshot in downloading_state_oneshot_vec.into_iter() {
                oneshot.send(state.clone()).unwrap_or_else(|_| {
                    #[cfg(feature = "tracing")]
                    tracing::trace!("send download_way failed!");
                });
            }

            let dec_result = match &state.download_way {
                DownloadWay::Ranges(item) => {
                    let request = Box::new(config.create_http_request());
                    item.start_download(
                        file,
                        request,
                        downloaded_len_change_notify,
                        true, // todo:,
                    )
                        .await
                }
                DownloadWay::Single(item) => {
                    item.download(
                        file,
                        Box::new(response),
                        downloaded_len_change_notify,
                        config.chunk_size.get(),
                    )
                        .await
                }
            };

            if { downloading_state.read().is_some() } {
                let mut guard = downloading_state.write();
                *guard = None;
            }

            let dec = dec_result?;

            end_sender.send(dec).unwrap_or_else(|_err| {
                #[cfg(feature = "tracing")]
                tracing::trace!("DownloadingEndCause Send Failed! {:?}", _err);
            });

            Ok(dec)
        }
    }
}

pub struct ExtendedHttpFileDownloader {
    pub inner: HttpFileDownloader,
    downloader_wrapper: Box<dyn DownloaderWrapper>,
}

impl ExtendedHttpFileDownloader {
    pub fn new(
        downloader: HttpFileDownloader,
        downloader_wrapper: Box<dyn DownloaderWrapper>,
    ) -> Self {
        Self {
            inner: downloader,
            downloader_wrapper,
        }
    }
    pub async fn prepare_download(&mut self) -> Result<DownloadFuture, DownloadStartError> {
        let download_future = self.inner.download().await?.boxed();
        Ok(self.downloader_wrapper.download(download_future).await?)
    }

    pub async fn download_to_end(&mut self) -> Result<DownloadingEndCause, DownloadToEndError> {
        Ok(self.prepare_download().await?.await?)
    }

    pub async fn cancel(&self) -> Result<(), DownloadStopError> {
        Ok(self.downloader_wrapper.cancel(self.inner.cancel().boxed()).await?)
    }

    #[inline]
    pub fn is_downloading(&self) -> bool {
        self.inner.is_downloading()
    }

    #[cfg(feature = "async-stream")]
    #[inline]
    pub async fn downloaded_len_stream(&self) -> impl Stream<Item=u64> {
        self.inner.downloaded_len_stream().await
    }

    #[inline]
    pub fn change_connection_count(
        &self,
        connection_count: NonZeroU8,
    ) -> Result<(), ChangeConnectionCountError> {
        self.inner.change_connection_count(connection_count)
    }
    #[inline]
    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) -> Result<(), ChangeChunkSizeError> {
        self.inner.change_chunk_size(chunk_size)
    }

    #[cfg(feature = "async-stream")]
    #[inline]
    pub async fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>>> {
        self.inner.chunks_stream().await
    }
    #[cfg(feature = "async-stream")]
    #[inline]
    pub async fn chunks_info_stream(&self) -> Option<impl Stream<Item=ChunksInfo>> {
        self.inner.chunks_info_stream().await
    }
    #[inline]
    pub fn downloaded_len(&self) -> u64 {
        self.inner.downloaded_len()
    }
    #[inline]
    pub async fn total_size(&self) -> Option<NonZeroU64> {
        self.inner.total_size().await
    }
    #[inline]
    pub fn current_total_size(&self) -> Option<NonZeroU64> {
        self.inner.current_total_size()
    }
    #[inline]
    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        self.inner.get_chunks().await
    }
    #[inline]
    pub fn get_file_path(&self) -> PathBuf {
        self.inner.get_file_path()
    }

    #[inline]
    pub fn config(&self) -> &HttpDownloadConfig {
        &self.inner.config
    }
    #[inline]
    pub fn downloaded_len_receiver(&self) -> &sync::watch::Receiver<u64> {
        &self.inner.downloaded_len_receiver
    }
}

use std::future::Future;
use std::io::SeekFrom;
use std::num::{NonZeroU64, NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use headers::{Header, HeaderMapExt};
use parking_lot::RwLock;
use thiserror::Error;
use tokio::{io, sync};
use tokio::io::AsyncSeekExt;
use tokio::sync::watch::error::SendError;
use tokio::task::JoinError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{ChunkData, ChunkItem, ChunkIterator, ChunkManager, ChunksInfo, DownloadArchiveData, DownloadedLenChangeNotify, DownloaderWrapper, DownloadFuture, DownloadWay, HttpDownloadConfig, HttpRedirectionHandle, RemainingChunks, SingleDownload};
use crate::exclusive::Exclusive;

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

#[derive(Debug, Copy, Clone)]
pub enum HttpResponseInvalidCause {
    ContentLengthInvalid,
    StatusCodeUnsuccessful,
    RedirectionNoLocation,
}

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("{:?}", .0)]
    Other(#[from] anyhow::Error),
    #[error("ArchiveDataLoadError {:?}", .0)]
    ArchiveDataLoadError(anyhow::Error),
    #[error("IoError，{:?}", .0)]
    IoError(#[from] io::Error),
    #[error("JoinError，{:?}", .0)]
    JoinError(#[from] JoinError),
    #[error("chunk remove failed，{:?}", .0)]
    ChunkRemoveFailed(usize),
    #[error("downloading chunk remove failed，{:?}", .0)]
    DownloadingChunkRemoveFailed(usize),
    #[error("http request failed，{:?}", .0)]
    HttpRequestFailed(#[from] reqwest::Error),
    #[error("http request response invalid，{:?}", .0)]
    HttpRequestResponseInvalid(HttpResponseInvalidCause, reqwest::Response),
    #[error("The server file has changed.")]
    ServerFileAlreadyChanged,
    #[error("The redirection times are too many.")]
    RedirectionTimesTooMany,
}

#[derive(Error, Debug)]
pub enum DownloadToEndError {
    #[error("{:?}", .0)]
    DownloadStartError(#[from] DownloadStartError),
    #[error("{:?}", .0)]
    DownloadError(#[from] DownloadError),
}

#[derive(Error, Debug)]
pub enum ChangeConnectionCountError {
    #[error("SendError")]
    SendError(#[from] SendError<u8>),
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

#[cfg(feature = "breakpoint-resume")]
#[derive(Default)]
pub struct BreakpointResume {
    pub data_archive_notify: sync::Notify,
    pub archive_complete_notify: sync::Notify,
}

pub struct HttpFileDownloader {
    pub downloading_state_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadingState>>>,
    pub downloaded_len_change_notify: Option<Arc<dyn DownloadedLenChangeNotify>>,
    pub archive_data_future: Option<Exclusive<BoxFuture<'static, Result<Option<Box<DownloadArchiveData>>>>>>,
    #[cfg(feature = "breakpoint-resume")]
    pub breakpoint_resume: Option<Arc<BreakpointResume>>,
    pub config: Arc<HttpDownloadConfig>,
    pub downloaded_len_receiver: sync::watch::Receiver<u64>,
    pub content_length: Arc<AtomicU64>,
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
        let cancel_token = config.cancel_token.clone().unwrap_or_default();
        let (downloaded_len_sender, downloaded_len_receiver) = sync::watch::channel::<u64>(0);
        let total_size_semaphore = Arc::new(sync::Semaphore::new(0));

        Self {
            downloading_state_oneshot_vec: vec![],
            downloaded_len_change_notify: None,
            archive_data_future: None,
            #[cfg(feature = "breakpoint-resume")]
            breakpoint_resume: None,
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
    pub fn downloaded_len_stream(&self) -> impl Stream<Item=u64> + 'static {
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
    pub fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>> + 'static> {
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
    pub fn chunks_info_stream(&self) -> Option<impl Stream<Item=ChunksInfo> + 'static> {
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

    pub fn total_size_future(&self) -> impl Future<Output=Option<NonZeroU64>> + 'static {
        let total_size_semaphore = self.total_size_semaphore.clone();
        let content_length = self.content_length.clone();
        async move {
            let _ = total_size_semaphore.acquire().await;
            let content_length = content_length.load(Ordering::Relaxed);
            if content_length == 0 {
                None
            } else {
                Some(NonZeroU64::new(content_length).unwrap())
            }
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

    pub fn get_chunk_manager(&self) -> Option<Weak<ChunkManager>> {
        self.get_downloading_state().and_then(|n|n.upgrade()).and_then(|downloading_state| {
            if let DownloadWay::Ranges(item) = &downloading_state.download_way {
                Some(Arc::downgrade(item))
            } else {
                None
            }
        })
    }
    pub fn get_downloading_state(&self) -> Option<Weak<DownloadingState>> {
        let guard = &self.downloading_state.read();
        guard.as_ref().map(|n| Arc::downgrade(&n.1))
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        match self.get_chunk_manager().and_then(|n|n.upgrade()) {
            None => Vec::new(),
            Some(n) => n.get_chunks().await,
        }
    }

    pub async fn get_chunks_info(&self) -> Option<ChunksInfo> {
        match self.get_chunk_manager().and_then(|n|n.upgrade()) {
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

    pub(crate) fn download(
        &mut self,
    ) -> Result<
        impl Future<Output=Result<DownloadingEndCause, DownloadError>> + Send + 'static,
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

    pub fn cancel(&self) -> impl Future<Output=()> + 'static {
        let downloading_state = self.downloading_state.clone();
        let token = self.cancel_token.clone();
        async move {
            let (receiver, _) = {
                let mut guard = downloading_state.write();
                let option = guard.take();
                if option.is_none() {
                    return;
                }
                option.unwrap()
            };
            token.cancel();
            receiver.await.unwrap();
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
        let archive_data_future = self.archive_data_future.take();
        let downloading_state_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadingState>>> = self.downloading_state_oneshot_vec.drain(..).collect();
        let downloaded_len_sender = self.downloaded_len_sender.clone();
        let cancel_token = self.cancel_token.clone();
        #[cfg(feature = "breakpoint-resume")]
            let breakpoint_resume = self.breakpoint_resume.take();


        async move {
            fn request<'a>(client: &'a reqwest::Client, config: &'a HttpDownloadConfig, location: Option<String>, redirection_times: usize) -> BoxFuture<'a, Result<(reqwest::Response, Option<String>), DownloadError>> {
                async move {
                    if let HttpRedirectionHandle::RequestNewLocation { max_times } = config.handle_redirection {
                        if redirection_times >= max_times {
                            return Err(DownloadError::RedirectionTimesTooMany);
                        }
                    }
                    let mut retry_count = 0;
                    let response = loop {
                        let response = client.execute(config.create_http_request(location.as_ref().map(|n| n.as_str())))
                            .await.and_then(|n| n.error_for_status());

                        if response.is_err() && retry_count < config.request_retry_count {
                            retry_count += 1;
                            #[cfg(feature = "tracing")]
                            tracing::trace!(
                            "Request error! {:?},retry_info: {}/{}",
                            response.unwrap_err(),
                            retry_count,
                            config.request_retry_count
                        );
                            continue;
                        }
                        break response;
                    };
                    // todo: 删除重定向，reqwest 本身可以处理重定向
                    match response {
                        Ok(response) if config.handle_redirection != HttpRedirectionHandle::Invalid && response.status().is_redirection() => {
                            let Some(location) = response.headers().get(headers::Location::name()) else {
                                return Err(DownloadError::HttpRequestResponseInvalid(HttpResponseInvalidCause::RedirectionNoLocation, response));
                            };
                            let Ok(location) = location.to_str().map(|n| n.to_string()) else {
                                return Err(DownloadError::HttpRequestResponseInvalid(HttpResponseInvalidCause::RedirectionNoLocation, response));
                            };
                            println!("handle_redirection!!!!!!! {}",location);
                            request(client, config, Some(location), redirection_times + 1).await
                        }
                        Ok(response) if !response.status().is_success() => {
                            Err(DownloadError::HttpRequestResponseInvalid(HttpResponseInvalidCause::StatusCodeUnsuccessful, response))
                        }
                        Err(err) => {
                            Err(DownloadError::HttpRequestFailed(err))
                        }
                        Ok(response) => Ok((response, location)),
                    }
                }.boxed()
            }

            let (end_sender, end_receiver) = sync::oneshot::channel();
            let dec = {
                let (response, location) = match request(&client, &config, None, 0).await {
                    Ok(r) => r,
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
                let content_length = response
                    .headers()
                    .typed_get::<headers::ContentLength>()
                    .map(|n| n.0)
                    .and_then(|n| if n == 0 { None } else { Some(n) });

                if let Some(0) = content_length {
                    total_size_semaphore.add_permits(1);
                    return Err(DownloadError::HttpRequestResponseInvalid(HttpResponseInvalidCause::ContentLengthInvalid, response));
                }
                content_length_arc.store(content_length.unwrap_or(0), Ordering::Relaxed);

                let accept_ranges = response.headers().typed_get::<headers::AcceptRanges>();

                let is_ranges_bytes_none = accept_ranges.is_none();
                let is_ranges_bytes =
                    !is_ranges_bytes_none && accept_ranges.unwrap() == headers::AcceptRanges::bytes();
                let archive_data = match archive_data_future {
                    None => { None }
                    Some(archive_data_future) => {
                        archive_data_future.await.map_err(DownloadError::ArchiveDataLoadError)?
                    }
                };
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
                        // download way changed? reset archive_data?
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


                let state = Arc::new(state);
                {
                    let mut guard = downloading_state.write();
                    *guard = Some((end_receiver, state.clone()));
                }

                total_size_semaphore.add_permits(1);

                let file = {
                    let mut options = std::fs::OpenOptions::new();
                    (config.open_option)(&mut options);
                    let mut file = tokio::fs::OpenOptions::from(options)
                        .open(config.file_path())
                        .await?;
                    if config.set_len_in_advance {
                        file.set_len(content_length.unwrap()).await?
                    }
                    file.seek(SeekFrom::Start(0)).await?;
                    file
                };

                for oneshot in downloading_state_oneshot_vec.into_iter() {
                    oneshot.send(state.clone()).unwrap_or_else(|_| {
                        #[cfg(feature = "tracing")]
                        tracing::trace!("send download_way failed!");
                    });
                }

                let dec_result = match &state.download_way {
                    DownloadWay::Ranges(item) => {
                        let request = Box::new(config.create_http_request(location.as_ref().map(|n| n.as_str())));
                        item.start_download(
                            file,
                            request,
                            downloaded_len_change_notify,
                            #[cfg(feature = "breakpoint-resume")]
                                breakpoint_resume,
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

                if {
                    let r = downloading_state.read().is_some();
                    r
                } {
                    let mut guard = downloading_state.write();
                    *guard = None;
                }

                dec_result?
            };

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

    /// 准备下载，返回了用于下载用的 'static 的 Future
    pub fn prepare_download(&mut self) -> Result<DownloadFuture, DownloadStartError> {
        self.downloader_wrapper.prepare_download(&mut self.inner)?;
        let prepare_download_result = self.inner.download();
        let download_future = self.downloader_wrapper.handle_prepare_download_result(&mut self.inner, prepare_download_result.map(|n| n.boxed()))?;

        self.downloader_wrapper.download(&mut self.inner, download_future)
    }

    /// 取消下载
    pub fn cancel(&self) -> impl Future<Output=()> + 'static {
        let cancel = self.downloader_wrapper.on_cancel();
        let cancel_future = self.inner.cancel();
        async move {
            cancel.await;
            cancel_future.await;
        }
    }

    /// 是否正在下载
    #[inline]
    pub fn is_downloading(&self) -> bool {
        self.inner.is_downloading()
    }

    /// 已下载长度流
    #[cfg(feature = "async-stream")]
    #[inline]
    pub fn downloaded_len_stream(&self) -> impl Stream<Item=u64> + 'static {
        self.inner.downloaded_len_stream()
    }

    /// 更改连接数
    #[inline]
    pub fn change_connection_count(
        &self,
        connection_count: NonZeroU8,
    ) -> Result<(), ChangeConnectionCountError> {
        self.inner.change_connection_count(connection_count)
    }

    /// 更改 chunk 大小
    #[inline]
    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) -> Result<(), ChangeChunkSizeError> {
        self.inner.change_chunk_size(chunk_size)
    }

    /// chunks 流，如果还真正的开始下载（获取了请求响应内容）会返回 None，可通过 `total_size_future().await` 等待获取它，避免得到 None
    #[cfg(feature = "async-stream")]
    #[inline]
    pub fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>> + 'static> {
        self.inner.chunks_stream()
    }

    /// chunks 信息流，如果还真正的开始下载（获取了请求响应内容）会返回 None，可通过 `total_size_future().await` 等待获取它，避免得到 None
    #[cfg(feature = "async-stream")]
    #[inline]
    pub fn chunks_info_stream(&self) -> Option<impl Stream<Item=ChunksInfo>> {
        self.inner.chunks_info_stream()
    }

    /// 已下载长度
    #[inline]
    pub fn downloaded_len(&self) -> u64 {
        self.inner.downloaded_len()
    }

    /// 总大小，会等待服务器响应，如果文件无大小则返回 None
    #[inline]
    pub fn total_size_future(&self) -> impl Future<Output=Option<NonZeroU64>> + 'static {
        self.inner.total_size_future()
    }

    /// 总大小，如果文件无大小或者还没有得到服务器响应时返回 None
    #[inline]
    pub fn current_total_size(&self) -> Option<NonZeroU64> {
        self.inner.current_total_size()
    }

    /// 总大小的`Arc`引用
    #[inline]
    pub fn atomic_total_size(&self) -> Arc<AtomicU64> {
        self.inner.content_length.clone()
    }

    /// 获取 chunks
    #[inline]
    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        self.inner.get_chunks().await
    }

    /// 获取文件路径
    #[inline]
    pub fn get_file_path(&self) -> PathBuf {
        self.inner.get_file_path()
    }

    /// 获取 DownloadingState，如果下载没有开始则返回 None
    #[inline]
    pub fn get_downloading_state(&self) -> Option<Weak<DownloadingState>> {
        self.inner.get_downloading_state()
    }

    /// 配置
    #[inline]
    pub fn config(&self) -> &HttpDownloadConfig {
        &self.inner.config
    }

    /// 已下载长度接收器
    #[inline]
    pub fn downloaded_len_receiver(&self) -> &sync::watch::Receiver<u64> {
        &self.inner.downloaded_len_receiver
    }

    /// DownloadingState 接收器
    pub fn downloading_state_receiver(&mut self) -> sync::oneshot::Receiver<Arc<DownloadingState>> {
        let (sender, receiver) = sync::oneshot::channel();
        self.inner.downloading_state_oneshot_vec.push(sender);
        receiver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_send<T: Send>() {}

    fn sync_sync<T: Sync>() {}

    #[test]
    fn assert_sync_send() {
        sync_send::<HttpFileDownloader>();
        sync_sync::<HttpFileDownloader>();
        sync_send::<ExtendedHttpFileDownloader>();
        sync_sync::<ExtendedHttpFileDownloader>();
    }
}
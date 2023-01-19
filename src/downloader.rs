use std::future::Future;
use std::io::SeekFrom;
use std::num::{NonZeroU64, NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
#[cfg(feature = "async-stream")]
use futures_util::Stream;
use headers::{HeaderMapExt, HeaderValue};
use parking_lot::RwLock;
use reqwest::header;
use thiserror::Error;
use tokio::{io, sync};
use tokio::fs::File;
use tokio::io::AsyncSeekExt;
use tokio::sync::Mutex;
#[cfg(feature = "status-tracker")]
use tokio::sync::watch::error::SendError;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "tracing")]
use tracing::{error, info_span, Instrument, warn};

use crate::{ChunkData, ChunkItem, ChunkIterator, ChunkManager, DownloadController, DownloadParams, DownloadWay, HttpDownloadConfig, RemainingChunks, SingleDownload};
#[cfg(feature = "status-tracker")]
use crate::status_tracker::{DownloaderStatus, NetworkItemPendingType};

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum DownloadingEndCause {
    DownloadFinished,
    Cancelled,
}

#[derive(Error, Debug)]
pub enum DownloadStartError {
    #[error("file create failed，{:?}", .0)]
    FileCrateFailed(#[from] io::Error),
    #[error("http request failed，{:?}", .0)]
    HttpRequestFailed(#[from] reqwest::Error),
    #[error("already downloading")]
    AlreadyDownloading,

    #[error("{:?}", .0)]
    Other(#[from] anyhow::Error),

    #[cfg(feature = "status-tracker")]
    #[error("pending")]
    Pending(NetworkItemPendingType),

    #[error("already download finished")]
    AlreadyDownloadFinished,
    #[cfg(feature = "status-tracker")]
    #[error("status send failed，{:?}", .0)]
    StatusSendFailed(#[from] SendError<DownloaderStatus>),
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
    pub end_receiver: sync::oneshot::Receiver<DownloadingEndCause>,
    pub download_way: Arc<DownloadWay>,
}

impl DownloadingState {}

pub struct HttpFileDownloader {
    pub config: Box<HttpDownloadConfig>,
    pub downloaded_len_receiver: sync::watch::Receiver<u64>,
    content_length: AtomicU64,
    client: reqwest::Client,
    downloading_state: RwLock<Option<DownloadingState>>,
    downloaded_len_sender: Arc<sync::watch::Sender<u64>>,
    pub cancel_token: Mutex<CancellationToken>,
    total_size_semaphore: Arc<sync::Semaphore>,
}

impl HttpFileDownloader {
    pub fn new(
        client: reqwest::Client,
        config: Box<HttpDownloadConfig>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let (downloaded_len_sender, downloaded_len_receiver) = sync::watch::channel::<u64>(0);
        let total_size_semaphore = Arc::new(sync::Semaphore::new(0));

        Self {
            config,
            total_size_semaphore,
            content_length: Default::default(),
            client,
            downloading_state: RwLock::new(None),
            downloaded_len_receiver,
            downloaded_len_sender: Arc::new(downloaded_len_sender),
            cancel_token: Mutex::new(cancel_token),
        }
    }

    pub fn is_downloading(&self) -> bool {
        self.downloading_state.read().is_some()
    }

    pub fn change_connection_count(&self, connection_count: NonZeroU8) -> Result<(), ChangeConnectionCountError> {
        match self.downloading_state.read().as_ref() {
            None => { Err(ChangeConnectionCountError::NoStart) }
            Some(r) => match r.download_way.as_ref() {
                DownloadWay::Single(_) => Err(ChangeConnectionCountError::DownloadTargetNotSupported),
                DownloadWay::Ranges(chunk_manager) => {
                    chunk_manager.change_connection_count(connection_count)?;
                    Ok(())
                }
            },
        }
    }
    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) -> Result<(), ChangeChunkSizeError> {
        match self.downloading_state.read().as_ref() {
            None => { Err(ChangeChunkSizeError::NoStart) }
            Some(r) => match r.download_way.as_ref() {
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
        async_stream::stream! {
            let downloaded_len = *downloaded_len_receiver.borrow();
            yield downloaded_len;
            while downloaded_len_receiver.changed().await.is_ok() {
                let downloaded_len = *downloaded_len_receiver.borrow();
                yield downloaded_len;
                if let Some(duration) = self.config.downloaded_len_send_interval{
                   tokio::time::sleep(duration).await;
                }
            }
        }
    }

    #[cfg(feature = "async-stream")]
    pub async fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>>> {
        match self.downloading_state.read().as_ref() {
            None => None,
            Some(r) => match r.download_way.as_ref() {
                DownloadWay::Single(_) => None,
                DownloadWay::Ranges(chunk_manager) => {
                    let mut downloaded_len_receiver = self.downloaded_len_receiver.clone();
                    let chunk_manager = chunk_manager.to_owned();
                    Some(async_stream::stream! {
                          yield chunk_manager.get_chunks().await;
                          while downloaded_len_receiver.changed().await.is_ok() {
                              yield chunk_manager.get_chunks().await;
                              if let Some(duration) = self.config.chunks_send_interval{
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
        self.total_size_semaphore.add_permits(1);
        let content_length = self.content_length.load(Ordering::Relaxed);
        if content_length == 0 {
            None
        } else {
            Some(NonZeroU64::new(content_length).unwrap())
        }
    }

    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        let chunk_manager = {
            let guard = self.downloading_state.read();
            guard.as_ref().and_then(|r| {
                if let DownloadWay::Ranges(item) = r.download_way.as_ref() {
                    Some(item.clone())
                } else {
                    None
                }
            })
        };
        match chunk_manager {
            None => Vec::new(),
            Some(n) => n.get_chunks().await,
        }
    }

    pub fn get_file_path(&self) -> PathBuf {
        self.config.file_path()
    }

    fn reset(&self) {
        self.downloaded_len_sender.send(0)
            .unwrap_or_else(|_err| {
                #[cfg(feature = "tracing")]
                error!("send downloaded_len failed! {}", _err);
            });
    }

    pub(crate) async fn download(
        self: Arc<Self>,
        params: DownloadParams,
    ) -> Result<impl Future<Output=Result<DownloadingEndCause, DownloadError>> + 'static, DownloadStartError>
    {
        self.reset();
        if self.is_downloading() {
            return Err(DownloadStartError::AlreadyDownloading);
        }

        let file = tokio::fs::OpenOptions::new().write(true).create(true).open(self.get_file_path()).await?;
        Ok(self.start_download(file, params))
    }

    pub fn downloading_state(&self) -> Option<DownloadingState> {
        let mut guard = self.downloading_state.write();
        guard.take()
    }

    pub async fn cancel(&self) -> Result<(), DownloadStopError> {
        {
            let mut cancel_token = self.cancel_token.lock().await;
            cancel_token.cancel();
            *cancel_token = CancellationToken::new();
        }
        if let Some(download_way) = self.downloading_state() {
            let handle = download_way.end_receiver;
            match handle.await? {
                DownloadingEndCause::DownloadFinished => {
                    Err(DownloadStopError::DownloadAlreadyFinished)
                }
                DownloadingEndCause::Cancelled => {
                    Ok(())
                }
            }
        } else {
            Err(DownloadStopError::NoStart)
        }
    }

    fn get_download_request(&self) -> reqwest::Request {
        let mut request = reqwest::Request::new(reqwest::Method::GET, (*self.config.url).clone());
        let header_map = request.headers_mut();
        header_map.insert(header::ACCEPT, HeaderValue::from_str("*/*").unwrap());
        header_map.typed_insert(headers::Connection::keep_alive());
        for (header_name, header_value) in self.config.header_map.iter() {
            header_map.insert(header_name, header_value.clone());
        }
        *request.timeout_mut() = self.config.timeout;
        request
    }

    //noinspection RsExternalLinter
    #[inline]
    async fn start_download(
        self: Arc<Self>,
        mut file: File,
        DownloadParams {
            downloaded_len_change_notify,
            archive_data,
            download_way_oneshot_vec,
            breakpoint_resume,
            ..
        }: DownloadParams,
    ) -> Result<DownloadingEndCause, DownloadError> {
        let cancel_token = self.cancel_token.lock().await.clone();
        let request = self.get_download_request();
        let response = self.client.execute(request);
        #[cfg(feature = "tracing")]
            let response = response.instrument(info_span!("request for content_length"));
        let response = response.await;
        if response.is_err() {
            self.total_size_semaphore.add_permits(1);
        }
        let response = response?;
        let etag = {
            if self.config.etag.is_some() {
                let etag = response.headers().typed_get::<headers::ETag>();
                if etag == self.config.etag {
                    error!("current etag: {:?} , target etag:{:?}", self.config.etag, etag);
                    return Err(DownloadError::ServerFileAlreadyChanged);
                }
                etag
            }else {
                None
            }
        };
        let content_length = response.headers().typed_get::<headers::ContentLength>().map(|n| n.0);
        let accept_ranges = response.headers().typed_get::<headers::AcceptRanges>();

        if let Some(content_length) = content_length {
            if content_length == 0 {
                return Err(DownloadError::ContentLengthInvalid);
            }
            file.set_len(content_length).await?;
        }
        self.content_length.store(content_length.unwrap_or(0), Ordering::Relaxed);
        self.total_size_semaphore.add_permits(1);

        let (end_sender, end_receiver) = sync::oneshot::channel();

        let download_way = {
            if content_length.is_some() && accept_ranges.is_some() {
                let content_length = content_length.unwrap();
                let chunk_data = archive_data
                    .and_then(|archive_data| {
                        self.downloaded_len_sender
                            .send(archive_data.downloaded_len)
                            .unwrap_or_else(|_err| {
                                #[cfg(feature = "tracing")]
                                error!("send downloaded_len failed! {}", _err);
                            });
                        archive_data.chunk_data.map(|mut data| {
                            if data.remaining.chunk_size != self.config.chunk_size {
                                #[cfg(feature = "tracing")]
                                tracing::trace!("ChunkSize Changed!");
                                data.remaining.chunk_size = self.config.chunk_size;
                            }
                            data
                        })
                    })
                    .unwrap_or_else(|| ChunkData {
                        iter_count: 0,
                        remaining: RemainingChunks::new(self.config.chunk_size, content_length),
                        last_incomplete_chunks: Default::default(),
                    });

                let chunk_iterator = ChunkIterator::new(content_length, chunk_data);
                let chunk_manager = Arc::new(ChunkManager::new(
                    self.config.download_connection_count,
                    self.client.clone(),
                    cancel_token,
                    self.downloaded_len_sender.clone(),
                    chunk_iterator,
                    etag,
                    self.config.request_retry_count,
                ));
                DownloadWay::Ranges(chunk_manager)
            } else {
                DownloadWay::Single(SingleDownload::new(
                    cancel_token,
                    self.downloaded_len_sender.clone(),
                ))
            }
        };
        file.seek(SeekFrom::Start(0)).await?;
        let file = Arc::new(Mutex::new(file));

        let download_way = Arc::new(download_way);

        {
            let mut guard = self.downloading_state.write();
            *guard = Some(DownloadingState {
                end_receiver,
                download_way: download_way.clone(),
            });
        }

        for oneshot in download_way_oneshot_vec {
            oneshot.send(download_way.clone()).unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                error!("send download_way failed!");
            });
        }

        let dec = match download_way.as_ref() {
            DownloadWay::Ranges(item) => {
                let request = Box::new(self.get_download_request());
                item.start_download(file, request, downloaded_len_change_notify, breakpoint_resume).await?
            }
            DownloadWay::Single(item) => {
                item.download(file, Box::new(response), self.config.request_retry_count, downloaded_len_change_notify)
                    .await?
            }
        };

        end_sender.send(dec).map_err(|err| {
            #[cfg(feature = "tracing")]
            warn!("DownloadingEndCause Send Failed! {:?}", err);
            anyhow::Error::msg("DownloadingEndCause Send Failed")
        })?;
        {
            let mut guard = self.downloading_state.write();
            *guard = None;
        }

        Ok(dec)
    }
}

#[async_trait]
impl DownloadController for HttpFileDownloader {
    async fn download(
        self: Arc<Self>,
        params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        Ok(HttpFileDownloader::download(self, params).await?.boxed())
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        Ok(self.cancel().await?)
    }
}

#[derive(Clone)]
pub struct ExtensibleHttpFileDownloader {
    pub inner: Arc<HttpFileDownloader>,
    download_controller: Arc<dyn DownloadController>,
}

impl ExtensibleHttpFileDownloader {
    pub fn new(
        downloader: Arc<HttpFileDownloader>,
        download_controller: Arc<dyn DownloadController>,
    ) -> Self {
        Self {
            inner: downloader,
            download_controller,
        }
    }

    pub async fn start(
        &self,
    ) -> Result<impl Future<Output=Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        let params = DownloadParams::new();
        let controller = self.download_controller.to_owned();
        let future = controller.download(params).await?;
        let r = tokio::spawn(async move {
            future.await
        });
        Ok(async {
            r.await?
        })
    }
    pub async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.download_controller.cancel().await?;
        Ok(())
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
    pub fn change_connection_count(&self, connection_count: NonZeroU8) -> Result<(), ChangeConnectionCountError> { self.inner.change_connection_count(connection_count) }
    #[inline]
    pub fn change_chunk_size(&self, chunk_size: NonZeroUsize) -> Result<(), ChangeChunkSizeError> { self.inner.change_chunk_size(chunk_size) }

    #[cfg(feature = "async-stream")]
    #[inline]
    pub async fn chunks_stream(&self) -> Option<impl Stream<Item=Vec<Arc<ChunkItem>>>> {
        self.inner.chunks_stream().await
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
    pub async fn get_chunks(&self) -> Vec<Arc<ChunkItem>> {
        self.inner.get_chunks().await
    }
    #[inline]
    pub fn get_file_path(&self) -> PathBuf {
        self.inner.get_file_path()
    }

    #[inline]
    pub fn config(&self) -> &HttpDownloadConfig { &self.inner.config }
    #[inline]
    pub fn downloaded_len_receiver(&self) -> &sync::watch::Receiver<u64> { &self.inner.downloaded_len_receiver }
    #[inline]
    pub fn downloading_state(&self) -> Option<DownloadingState> { self.inner.downloading_state() }
}

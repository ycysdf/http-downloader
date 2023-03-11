use std::io::SeekFrom;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use bytes::Bytes;
use futures_util::future::{BoxFuture, OptionFuture};
use futures_util::StreamExt;
use headers::HeaderMapExt;
use reqwest::Request;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::select;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "tracing")]
use tracing::Instrument;

use crate::{ChunkInfo, ChunkManager, ChunkRange, DownloadError, DownloadingEndCause};

pub trait DownloadedLenChangeNotify: Send + Sync {
    fn receive_len(&self, len: usize) -> OptionFuture<BoxFuture<()>>;
}

pub struct ChunkItem {
    pub chunk_info: ChunkInfo,
    pub downloaded_len: AtomicU64,
    cancel_token: CancellationToken,
    client: reqwest::Client,
    file: Arc<Mutex<File>>,
    etag: Option<headers::ETag>,
}

impl ChunkItem {
    pub fn new(
        chunk_info: ChunkInfo,
        cancel_token: CancellationToken,
        client: reqwest::Client,
        file: Arc<Mutex<File>>,
        etag: Option<headers::ETag>,
    ) -> Self {
        Self {
            downloaded_len: AtomicU64::new(0),
            cancel_token,
            client,
            chunk_info,
            file,
            etag,
        }
    }

    #[inline]
    fn add_downloaded_len(&self, len: usize) {
        self.downloaded_len.fetch_add(len as u64, Ordering::Relaxed);
        debug_assert!(
            self.downloaded_len.load(Ordering::SeqCst) <= self.chunk_info.range.len(),
            "downloaded_len:{},chunk_info.range.len():{}",
            self.downloaded_len.load(Ordering::SeqCst),
            self.chunk_info.range.len()
        );
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(name = "download chunk", skip_all, fields(chunk_index = self.chunk_info.index)))]
    pub(crate) async fn download_chunk(
        self: Arc<Self>,
        mut request: Box<Request>,
        retry_count: u8,
        downloaded_len_receiver: Option<impl DownloadedLenChangeNotify>,
    ) -> Result<DownloadingEndCause, DownloadError> {
        let cancel_token = self.cancel_token.clone();
        let mut chunk_bytes = Vec::with_capacity(self.chunk_info.range.len() as usize);

        let mut cur_retry_count = 0;
        let future = async {
            'r: loop {
                request.headers_mut().typed_insert(
                    ChunkRange::new(
                        self.chunk_info.range.start + chunk_bytes.len() as u64,
                        self.chunk_info.range.end,
                    )
                        .to_range_header(),
                );
                // 避免 clone request ?
                let response = self.client.execute(*ChunkManager::clone_request(&request));
                #[cfg(feature = "tracing")]
                    let response = response.instrument(tracing::info_span!("chunk's http request"));
                let response = match response.await {
                    Ok(response) => {
                        cur_retry_count = 0;
                        response
                    }
                    Err(err) => {
                        cur_retry_count += 1;
                        #[cfg(feature = "tracing")]
                        tracing::trace!(
                            "Request error! {:?},retry_info: {}/{}",
                            err,
                            cur_retry_count,
                            retry_count
                        );
                        if cur_retry_count > retry_count {
                            return Err(DownloadError::HttpRequestFailed(err));
                        }
                        continue 'r;
                    }
                };
                if self.etag.is_some() {
                    let etag = response.headers().typed_get::<headers::ETag>();
                    if etag != self.etag {
                        #[cfg(feature = "tracing")]
                        tracing::trace!(
                            "etag mismatching,your etag: {:?} , current etag:{:?}",
                            self.etag,
                            etag
                        );
                        return Err(DownloadError::ServerFileAlreadyChanged);
                    }
                }
                let mut stream = response.bytes_stream();
                while let Some(bytes) = stream.next().await {
                    #[cfg(feature = "tracing")]
                        let span = tracing::info_span!("process received bytes", is_ok = bytes.is_ok());
                    #[cfg(feature = "tracing")]
                        let _ = span.enter();
                    let bytes: Bytes = {
                        match bytes {
                            Ok(bytes) => {
                                cur_retry_count = 0;
                                bytes
                            }
                            Err(err) => {
                                cur_retry_count += 1;
                                #[cfg(feature = "tracing")]
                                tracing::trace!(
                                    "Request error! {:?},retry_info: {}/{}",
                                    err,
                                    cur_retry_count,
                                    retry_count
                                );
                                if cur_retry_count > retry_count {
                                    // 出错后与取消一样处理：将缓冲中的数据写入磁盘并持久化数据
                                    let mut file = self.file.lock().await;
                                    file.seek(SeekFrom::Start(self.chunk_info.range.start))
                                        .await?;
                                    debug_assert!(
                                        chunk_bytes.len() as u64 <= self.chunk_info.range.len(),
                                        "chunk_bytes.len() = {}, self.chunk_info.range.len() = {}",
                                        chunk_bytes.len(),
                                        self.chunk_info.range.len()
                                    );
                                    file.write_all(chunk_bytes.as_ref()).await?;
                                    file.flush().await?;
                                    file.sync_all().await?;
                                    return Err(DownloadError::HttpRequestFailed(err));
                                }
                                continue 'r;
                            }
                        }
                    };
                    let len = bytes.len();
                    chunk_bytes.extend(bytes);
                    self.add_downloaded_len(len);
                    if let Some(downloaded_len_receiver) = downloaded_len_receiver.as_ref() {
                        downloaded_len_receiver.receive_len(len).await;
                    }
                }
                break;
            }
            Result::<(), DownloadError>::Ok(())
        };

        select! {
            r = future => {
                r?;
                let mut file = self.file.lock().await;
                file.seek(SeekFrom::Start(self.chunk_info.range.start)).await?;
                debug_assert_eq!(chunk_bytes.len() as u64,self.chunk_info.range.len());
                file.write_all(chunk_bytes.as_ref()).await?;
                file.flush().await?;
                file.sync_all().await?;
                Ok(DownloadingEndCause::DownloadFinished)
            }
            _ = cancel_token.cancelled() => {
                let mut file = self.file.lock().await;
                file.seek(SeekFrom::Start(self.chunk_info.range.start)).await?;
                debug_assert!(chunk_bytes.len() as u64 <= self.chunk_info.range.len(),"chunk_bytes.len() = {}, self.chunk_info.range.len() = {}", chunk_bytes.len(), self.chunk_info.range.len());
                file.write_all(chunk_bytes.as_ref()).await?;
                file.flush().await?;
                file.sync_all().await?;
                Ok(DownloadingEndCause::Cancelled)
            }
        }
    }
    /*
        pub(crate) fn start_download(
            self: Arc<Self>,
            request: Box<Request>,
            retry_count: u8,
        ) -> DownloadedChunkItem {
            use futures_util::FutureExt;

            let chunk_item = self.clone();
            let join_handle = tokio::spawn(self.download_chunk(request, retry_count).then(
                |result| async move {
                    match result {
                        Ok(is_finished) => {
                            if is_finished {
                                self.send_message(ChunkMessageKind::DownloadFinished)
                                    .await
                                    .unwrap_or_else(|_err| {
                                        #[cfg(feature = "tracing")]
                                        tracing::trace!("ChunkMessageInfoSendFailed! {:?}", _err);
                                    })
                            }
                        }
                        Err(err) => self
                            .send_message(ChunkMessageKind::Error(err))
                            .await
                            .unwrap_or_else(|_err| {
                                #[cfg(feature = "tracing")]
                                tracing::trace!("ChunkMessageInfoSendFailed! {:?}", _err);
                            }),
                    };
                },
            ));
            DownloadedChunkItem::new(chunk_item, join_handle)
        }*/
}

/*pub struct DownloadedChunkItem {
    pub chunk_item: Arc<ChunkItem>,
    pub join_handle: JoinHandle<()>,
}

impl DownloadedChunkItem {
    pub fn new(chunk_item: Arc<ChunkItem>, join_handle: JoinHandle<()>) -> Self {
        Self {
            chunk_item,
            join_handle,
        }
    }
}

impl Deref for DownloadedChunkItem {
    type Target = Arc<ChunkItem>;

    fn deref(&self) -> &Self::Target {
        &self.chunk_item
    }
}*/

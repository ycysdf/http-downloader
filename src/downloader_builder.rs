use std::borrow::Cow;
use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use headers::{ETag, HeaderMap};
use url::Url;

use crate::{DownloadController, DownloadExtension, ExtensibleHttpFileDownloader, HttpFileDownloader};

pub struct HttpDownloadConfig {
    pub download_connection_count: NonZeroU8,
    pub chunk_size: NonZeroUsize,
    pub file_name: String,
    pub url: Arc<Url>,
    pub save_dir: PathBuf,
    pub etag: Option<ETag>,
    pub request_retry_count: u8,
    pub timeout: Option<Duration>,
    pub header_map: HeaderMap,
}

impl HttpDownloadConfig {
    pub fn file_path(&self) -> PathBuf {
        self.save_dir.join(&self.file_name)
    }
}

pub struct HttpDownloaderBuilder {
    chunk_size: NonZeroUsize,
    download_connection_count: NonZeroU8,
    url: Url,
    save_dir: PathBuf,
    file_name: Option<String>,
    request_retry_count: u8,
    timeout: Option<Duration>,
    etag: Option<ETag>,
    client: Option<reqwest::Client>,
    header_map: HeaderMap,
}

impl HttpDownloaderBuilder {
    pub fn new(url: Url, save_dir: PathBuf) -> Self {
        Self {
            client: None,
            chunk_size: NonZeroUsize::new(1024 * 1024 * 4).unwrap(), // 4M,
            file_name: None,
            request_retry_count: 3,
            download_connection_count: NonZeroU8::new(3).unwrap(),
            url,
            save_dir,
            etag: None,
            timeout: None,
            header_map: Default::default(),
        }
    }

    pub fn client(mut self, client: Option<reqwest::Client>) -> Self {
        self.client = client;
        self
    }

    pub fn request_retry_count(mut self, request_retry_count: u8) -> Self {
        self.request_retry_count = request_retry_count;
        self
    }

    pub fn header_map(mut self, header_map: HeaderMap) -> Self {
        self.header_map = header_map;
        self
    }

    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn chunk_size(mut self, chunk_size: NonZeroUsize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    pub fn download_connection_count(mut self, download_connection_count: NonZeroU8) -> Self {
        self.download_connection_count = download_connection_count;
        self
    }

    pub fn build<
        DC: DownloadController + 'static,
        DE: DownloadExtension<HttpFileDownloader, DownloadController=DC>,
    >(
        self,
        extension: DE,
    ) -> (ExtensibleHttpFileDownloader, DE::ExtensionState) {
        let downloader = Arc::new(HttpFileDownloader::new(
            self.client.unwrap_or(Default::default()),
            Box::new(HttpDownloadConfig {
                download_connection_count: self.download_connection_count,
                chunk_size: self.chunk_size,
                file_name: self.file_name.unwrap_or_else(|| self.url.file_name().to_string()),
                url: Arc::new(self.url),
                save_dir: self.save_dir,
                etag: self.etag,
                request_retry_count: self.request_retry_count,
                timeout: self.timeout,
                header_map: self.header_map,
            }),
        ));
        let (ec, es) = extension.layer(downloader.clone(), downloader.clone());
        (ExtensibleHttpFileDownloader::new(downloader, ec), es)
    }
}

pub trait UrlFileName {
    fn file_name(&self) -> Cow<str>;
}

impl UrlFileName for Url {
    fn file_name(&self) -> Cow<str> {
        self.path_segments()
            .map(|n| {
                n.last().map(Cow::Borrowed).unwrap_or_else(|| {
                    self.domain()
                        .map(Cow::Borrowed)
                        .unwrap_or(Cow::Owned("index.html".to_string()))
                })
            })
            .unwrap_or_else(|| Cow::Owned("index.html".to_string()))
    }
}
use std::borrow::Cow;
use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use headers::{ETag, HeaderMap, HeaderMapExt};
use url::Url;

use crate::{DownloadExtensionBuilder, ExtendedHttpFileDownloader, HttpFileDownloader};

pub struct HttpDownloadConfig {
    // 提前设置长度，如果存储空间不足将提前报错
    pub set_len_in_advance: bool,
    pub download_connection_count: NonZeroU8,
    pub chunk_size: NonZeroUsize,
    pub save_dir: PathBuf,
    pub file_name: String,
    pub open_option: Box<dyn Fn(&mut std::fs::OpenOptions) + Send + Sync + 'static>,
    pub create_dir: bool,
    pub url: Arc<Url>,
    pub etag: Option<ETag>,
    pub request_retry_count: u8,
    // pub timeout: Option<Duration>,
    pub header_map: HeaderMap,
    pub downloaded_len_send_interval: Option<Duration>,
    pub chunks_send_interval: Option<Duration>,
    pub handle_zero_content: bool,
    pub strict_check_accept_ranges: bool,
    pub http_request_configure: Option<Box<dyn Fn(reqwest::Request) -> reqwest::Request + Send + Sync + 'static>>,
}

impl HttpDownloadConfig {
    pub fn file_path(&self) -> PathBuf {
        self.save_dir.join(&self.file_name)
    }


    pub fn create_http_request(&self) -> reqwest::Request {
        let mut request = reqwest::Request::new(reqwest::Method::GET, (*self.url).clone());
        let header_map = request.headers_mut();
        header_map.insert(reqwest::header::ACCEPT, headers::HeaderValue::from_str("*/*").unwrap());
        header_map.typed_insert(headers::Connection::keep_alive());
        for (header_name, header_value) in self.header_map.iter() {
            header_map.insert(header_name, header_value.clone());
        }
        // 限速后超时会出现异常
        *request.timeout_mut() = None;
        // *request.timeout_mut() = self.config.timeout;
        match self.http_request_configure.as_ref() {
            None => { request }
            Some(configure) => {
                configure(request)
            }
        }
    }
}

pub struct HttpDownloaderBuilder {
    chunk_size: NonZeroUsize,
    download_connection_count: NonZeroU8,
    url: Url,
    save_dir: PathBuf,
    set_len_in_advance: bool,
    file_name: Option<String>,
    open_option: Box<dyn Fn(&mut std::fs::OpenOptions) + Send + Sync + 'static>,
    create_dir: bool,
    request_retry_count: u8,
    // timeout: Option<Duration>,
    etag: Option<ETag>,
    client: Option<reqwest::Client>,
    header_map: HeaderMap,
    downloaded_len_send_interval: Option<Duration>,
    chunks_send_interval: Option<Duration>,
    handle_zero_content: bool,
    strict_check_accept_ranges: bool,
    http_request_configure: Option<Box<dyn Fn(reqwest::Request) -> reqwest::Request + Send + Sync + 'static>>,
}

impl HttpDownloaderBuilder {
    pub fn new(url: Url, save_dir: PathBuf) -> Self {
        Self {
            client: None,
            chunk_size: NonZeroUsize::new(1024 * 1024 * 4).unwrap(), // 4M,
            file_name: None,
            open_option: Box::new(|o| {
                o.create(true).write(true);
            }),
            create_dir: true,
            request_retry_count: 3,
            download_connection_count: NonZeroU8::new(3).unwrap(),
            url,
            save_dir,
            etag: None,
            // timeout: None,
            header_map: Default::default(),
            downloaded_len_send_interval: Some(Duration::from_millis(300)),
            chunks_send_interval: Some(Duration::from_millis(300)),
            handle_zero_content: false,
            strict_check_accept_ranges: true,
            http_request_configure: None,
            set_len_in_advance: false,
        }
    }

    pub fn client(mut self, client: Option<reqwest::Client>) -> Self {
        self.client = client;
        self
    }

    pub fn create_dir(mut self, create_dir: bool) -> Self {
        self.create_dir = create_dir;
        self
    }
    pub fn set_len_in_advance(mut self, set_len_in_advance: bool) -> Self {
        self.set_len_in_advance = set_len_in_advance;
        self
    }
    pub fn downloaded_len_send_interval(
        mut self,
        downloaded_len_send_interval: Option<Duration>,
    ) -> Self {
        self.downloaded_len_send_interval = downloaded_len_send_interval;
        self
    }
    pub fn chunks_send_interval(mut self, chunks_send_interval: Option<Duration>) -> Self {
        self.chunks_send_interval = chunks_send_interval;
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
    /*
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }*/

    pub fn file_name(mut self, file_name: Option<String>) -> Self {
        self.file_name = file_name;
        self
    }

    pub fn chunk_size(mut self, chunk_size: NonZeroUsize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    pub fn etag(mut self, etag: Option<ETag>) -> Self {
        self.etag = etag;
        self
    }
    pub fn handle_zero_content(mut self, handle_zero_content: bool) -> Self {
        self.handle_zero_content = handle_zero_content;
        self
    }
    pub fn strict_check_accept_ranges(mut self, strict_check_accept_ranges: bool) -> Self {
        self.strict_check_accept_ranges = strict_check_accept_ranges;
        self
    }

    pub fn download_connection_count(mut self, download_connection_count: NonZeroU8) -> Self {
        self.download_connection_count = download_connection_count;
        self
    }

    pub fn http_request_configure(mut self, http_request_configure: impl Fn(reqwest::Request) -> reqwest::Request + Send + Sync + 'static) -> Self {
        self.http_request_configure = Some(Box::new(http_request_configure));
        self
    }

    pub fn build<
        DEB: DownloadExtensionBuilder,
    >(
        self,
        extension_builder: DEB,
    ) -> (ExtendedHttpFileDownloader, DEB::ExtensionState) {
        let mut downloader = HttpFileDownloader::new(
            self.client.unwrap_or(Default::default()),
            Arc::new(HttpDownloadConfig {
                set_len_in_advance: self.set_len_in_advance,
                download_connection_count: self.download_connection_count,
                chunk_size: self.chunk_size,
                file_name: self
                    .file_name
                    .unwrap_or_else(|| self.url.file_name().to_string()),
                open_option: self.open_option,
                create_dir: self.create_dir,
                url: Arc::new(self.url),
                save_dir: self.save_dir,
                etag: self.etag,
                request_retry_count: self.request_retry_count,
                // timeout: self.timeout,
                header_map: self.header_map,
                downloaded_len_send_interval: self.downloaded_len_send_interval,
                chunks_send_interval: self.chunks_send_interval,
                handle_zero_content: self.handle_zero_content,
                strict_check_accept_ranges: self.strict_check_accept_ranges,
                http_request_configure: self.http_request_configure,
            }),
        );
        let (extension, es) = extension_builder.build(&mut downloader);
        (ExtendedHttpFileDownloader::new(downloader, Box::new(extension)), es)
    }
}

pub trait UrlFileName {
    fn file_name(&self) -> Cow<str>;
}

impl UrlFileName for Url {
    fn file_name(&self) -> Cow<str> {
        let website_default: &'static str = "index.html";
        self.path_segments()
            .map(|n| {
                n.last()
                    .map(|n| Cow::Borrowed(if n.is_empty() { website_default } else { n }))
                    .unwrap_or_else(|| {
                        self.domain()
                            .map(Cow::Borrowed)
                            .unwrap_or(Cow::Owned(website_default.to_string()))
                    })
            })
            .unwrap_or_else(|| Cow::Owned(website_default.to_string()))
    }
}

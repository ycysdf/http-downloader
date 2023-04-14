use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::future::{BoxFuture, OptionFuture};
use futures_util::FutureExt;
use tokio::select;

use crate::{DownloadedLenChangeNotify, DownloaderWrapper, DownloadExtensionBuilder, DownloadFuture, DownloadingState, DownloadStartError, HttpFileDownloader};

pub struct DownloadSpeedLimiterExtension<Limiter: SpeedLimiter> {
    limiter: Arc<Limiter>,
}

impl DownloadSpeedLimiterExtension<DefaultSpeedLimiter> {
    pub fn new(byte_count_per: Option<usize>) -> Self {
        Self {
            limiter: Arc::new(DefaultSpeedLimiter::new(byte_count_per))
        }
    }
}

impl<Limiter: SpeedLimiter> DownloadSpeedLimiterExtension<Limiter> {
    pub fn from_limiter(limiter: Arc<Limiter>) -> Self {
        Self {
            limiter
        }
    }
}

#[derive(Clone)]
pub struct DownloadSpeedLimiterState<Limiter: SpeedLimiter> {
    speed_limiter: Arc<Limiter>,
}

impl<Limiter: SpeedLimiter> DownloadSpeedLimiterState<Limiter> {
    pub async fn change_speed(&self, byte_count_per: Option<usize>) {
        self.speed_limiter.change(byte_count_per).await;
    }
}

pub struct DownloadSpeedLimiterDownloaderWrapper<Limiter: SpeedLimiter> {
    limiter: Arc<Limiter>,
    pub receiver: Option<tokio::sync::oneshot::Receiver<Arc<DownloadingState>>>,
}

impl<Limiter: SpeedLimiter> DownloaderWrapper for DownloadSpeedLimiterDownloaderWrapper<Limiter> {
    fn prepare_download(&mut self, downloader: &mut HttpFileDownloader) -> Result<(), DownloadStartError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        downloader.downloaded_len_change_notify = Some(self.limiter.clone());
        downloader.downloading_state_oneshot_vec.push(sender);
        self.receiver = Some(receiver);
        Ok(())
    }
    fn download(&mut self, _downloader: &mut HttpFileDownloader, download_future: DownloadFuture) -> Result<DownloadFuture, DownloadStartError> {
        let receiver = self.receiver.take().unwrap();

        let limiter = self.limiter.clone();
        let future = async move {
            if receiver.await.is_ok(){
                limiter.reset().await;
            }
            futures_util::future::pending::<()>().await
        };

        Ok(async move {
            select! {
                _ = future => {unreachable!()},
                r = download_future => {
                    r
                }
            }
        }
            .boxed())
    }
}

impl<Limiter: SpeedLimiter> DownloadExtensionBuilder for DownloadSpeedLimiterExtension<Limiter> {
    type Wrapper = DownloadSpeedLimiterDownloaderWrapper<Limiter>;
    type ExtensionState = DownloadSpeedLimiterState<Limiter>;

    fn build(self, _downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) where Self: Sized {
        (
            DownloadSpeedLimiterDownloaderWrapper {
                limiter: self.limiter.clone(),
                receiver: None,
            },
            DownloadSpeedLimiterState { speed_limiter: self.limiter },
        )
    }
}

#[async_trait::async_trait]
pub trait SpeedLimiter: DownloadedLenChangeNotify + 'static {
    async fn change(&self, byte_count_per: Option<usize>);

    async fn reset(&self);
}

impl DownloadedLenChangeNotify for DefaultSpeedLimiter {
    #[inline]
    fn receive_len(&self, len: usize) -> OptionFuture<BoxFuture<()>> {
        let byte_count_per = self.byte_count_per.load(Ordering::Relaxed);
        // 0 表示不限速
        if byte_count_per == 0 {
            return None.into();
        }
        let cur_read = self.cur_read.fetch_add(len, Ordering::SeqCst);

        if cur_read < byte_count_per {
            return None.into();
        }

        Some(async move {
            let mut last_instant = self.last_instant.lock().await;
            if self.cur_read.load(Ordering::SeqCst) < byte_count_per {
                return;
            }
            let elapsed_millis = last_instant.elapsed();
            if elapsed_millis.as_millis() < LIMIT_INTERVAL as u128 {
                let duration = Duration::from_millis(LIMIT_INTERVAL) - elapsed_millis;
                // tracing::info!("sleep duration:{duration:?}");
                tokio::time::sleep(duration).await;
            }
            *last_instant = Instant::now();
            self.cur_read.fetch_sub(byte_count_per, Ordering::SeqCst);
        }.boxed()).into()
    }
}

#[async_trait::async_trait]
impl SpeedLimiter for DefaultSpeedLimiter {
    async fn change(&self, byte_count_per: Option<usize>) {
        self.byte_count_per.store(Self::handle_byte_count_per(byte_count_per), Ordering::Relaxed);
        self.reset().await;
    }

    async fn reset(&self) {
        let mut last_instant = self.last_instant.lock().await;
        self.cur_read.store(0, Ordering::Relaxed);
        *last_instant = Instant::now();
    }
}

#[derive(Debug)]
pub struct DefaultSpeedLimiter {
    byte_count_per: AtomicUsize,
    cur_read: AtomicUsize,
    last_instant: tokio::sync::Mutex<Instant>,
}

const LIMIT_INTERVAL: u64 = 1000;

impl DefaultSpeedLimiter {
    // 0 表示不限速
    pub fn new(byte_count_per: Option<usize>) -> Self {
        Self {
            byte_count_per: AtomicUsize::new(
                Self::handle_byte_count_per(byte_count_per)
            ),
            cur_read: Default::default(),
            last_instant: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    fn handle_byte_count_per(byte_count_per: Option<usize>) -> usize {
        byte_count_per.map(|n| ((n as i64) * (LIMIT_INTERVAL as i64 / 1000_i64)) as usize).unwrap_or(0)
    }
}

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::{DownloadController, DownloadedLenChangeNotify, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, HttpFileDownloader};

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

pub struct DownloadSpeedLimiterController<DC: DownloadController, Limiter: SpeedLimiter> {
    inner: Arc<DC>,
    speed_limiter: Arc<Limiter>,
}

impl<DC: DownloadController, Limiter: SpeedLimiter> DownloadExtension<DC> for DownloadSpeedLimiterExtension<Limiter> {
    type DownloadController = DownloadSpeedLimiterController<DC, Limiter>;
    type ExtensionState = DownloadSpeedLimiterState<Limiter>;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        drop(downloader);
        let speed_limiter = self.limiter;
        (
            Arc::new(DownloadSpeedLimiterController {
                inner,
                speed_limiter: speed_limiter.clone(),
            }),
            DownloadSpeedLimiterState { speed_limiter },
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
    fn receive_len(&self, len: usize) -> Option<BoxFuture<()>> {
        self.wait(len).map(|n| n.boxed())
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

    #[inline]
    pub fn wait(&self, len: usize) -> Option<impl Future<Output=()> + '_> {
        let byte_count_per = self.byte_count_per.load(Ordering::Relaxed);
        // 0 表示不限速
        if byte_count_per == 0 {
            return None;
        }
        let cur_read = self.cur_read.fetch_add(len, Ordering::SeqCst);

        if cur_read < byte_count_per {
            return None;
        }

        Some(async move {
            let mut last_instant = self.last_instant.lock().await;
            if self.cur_read.load(Ordering::SeqCst) < byte_count_per {
                return;
            }
            let elapsed_millis = last_instant.elapsed();
            if elapsed_millis.as_millis() < LIMIT_INTERVAL as u128 {
                tokio::time::sleep(Duration::from_millis(LIMIT_INTERVAL) - elapsed_millis).await;
            }
            *last_instant = Instant::now();
            self.cur_read.fetch_sub(byte_count_per, Ordering::SeqCst);
        })
    }
}

#[async_trait]
impl<DC: DownloadController, Limiter: SpeedLimiter> DownloadController for DownloadSpeedLimiterController<DC, Limiter> {
    async fn download(
        self: Arc<Self>,
        mut params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        params.downloaded_len_change_notify = Some(self.speed_limiter.clone());
        self.speed_limiter.reset().await;
        self.inner.to_owned().download(params).await
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.inner.cancel().await?;
        self.speed_limiter.reset().await;
        Ok(())
    }
}

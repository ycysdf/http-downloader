use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::sync::RwLock;

use crate::{DownloadController, DownloadedLenChangeNotify, DownloadError, DownloadExtension, DownloadingEndCause, DownloadParams, DownloadStartError, DownloadStopError, HttpFileDownloader};

pub struct DownloadSpeedLimiterExtension {
    pub byte_count_per: Option<usize>,
}

#[derive(Clone)]
pub struct DownloadSpeedLimiterState {
    speed_limiter: Arc<SpeedLimiter>,
}

impl DownloadSpeedLimiterState {
    pub async fn change_speed(&self, byte_count_per: Option<usize>) {
        self.speed_limiter.change(byte_count_per).await;
    }
}

pub struct DownloadSpeedLimiterController<DC: DownloadController> {
    inner: Arc<DC>,
    speed_limiter: Arc<SpeedLimiter>,
}

impl<DC: DownloadController> DownloadExtension<DC> for DownloadSpeedLimiterExtension {
    type DownloadController = DownloadSpeedLimiterController<DC>;
    type ExtensionState = DownloadSpeedLimiterState;

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        drop(downloader);
        let speed_limiter = Arc::new(SpeedLimiter::new(self.byte_count_per));
        (
            Arc::new(DownloadSpeedLimiterController {
                inner,
                speed_limiter: speed_limiter.clone(),
            }),
            DownloadSpeedLimiterState { speed_limiter },
        )
    }
}

impl DownloadedLenChangeNotify for SpeedLimiter {
    #[inline]
    fn receive_len(&self, len: usize) -> Option<BoxFuture<()>> {
        self.wait(len).map(|n| n.boxed())
    }
}

#[derive(Debug)]
pub struct SpeedLimiter {
    byte_count_per: AtomicUsize,
    cur_read: AtomicUsize,
    last_instant: RwLock<Instant>,
}

const LIMIT_INTERVAL: u64 = 1000;

impl SpeedLimiter {
    // 0 表示不限速
    pub fn new(byte_count_per: Option<usize>) -> Self {
        Self {
            byte_count_per: AtomicUsize::new(
                Self::handle_byte_count_per(byte_count_per)
            ),
            cur_read: Default::default(),
            last_instant: RwLock::new(Instant::now()),
        }
    }

    fn handle_byte_count_per(byte_count_per: Option<usize>) -> usize {
        byte_count_per.map(|n| ((n as i64) * (LIMIT_INTERVAL as i64 / 1000_i64)) as usize).unwrap_or(0)
    }

    pub async fn change(&self, byte_count_per: Option<usize>) {
        self.byte_count_per.store(Self::handle_byte_count_per(byte_count_per), Ordering::Relaxed);
        self.reset().await;
    }

    pub async fn reset(&self) {
        let mut last_instant = self.last_instant.write().await;
        self.cur_read.store(0, Ordering::Relaxed);
        *last_instant = Instant::now();
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
            let elapsed_millis = self.last_instant.read().await.elapsed();
            if elapsed_millis.as_millis() < LIMIT_INTERVAL as u128 {
                let mut last_instant = self.last_instant.write().await;
                if self.cur_read.load(Ordering::SeqCst) < byte_count_per {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(LIMIT_INTERVAL) - elapsed_millis).await;
                *last_instant = Instant::now();
                self.cur_read.fetch_sub(byte_count_per, Ordering::SeqCst);
            }
        })
    }
}

#[async_trait]
impl<DC: DownloadController> DownloadController for DownloadSpeedLimiterController<DC> {
    async fn download(
        self: Arc<Self>,
        mut params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError> {
        params.downloaded_len_change_notify = Some(self.speed_limiter.clone());
        self.inner.to_owned().download(params).await
    }

    async fn cancel(&self) -> Result<(), DownloadStopError> {
        self.inner.cancel().await?;
        self.speed_limiter.reset().await;
        Ok(())
    }
}

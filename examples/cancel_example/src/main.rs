use std::future::Future;
use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use futures_util::{FutureExt, StreamExt};
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use tracing::info;
use url::Url;

use http_downloader::{breakpoint_resume::DownloadBreakpointResumeExtension, DownloadError, DownloadFuture, DownloadingEndCause, HttpDownloaderBuilder, speed_tracker::DownloadSpeedTrackerExtension, status_tracker::DownloadStatusTrackerExtension};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://releases.ubuntu.com/22.04/ubuntu-22.04.2-desktop-amd64.iso")?;
    let (mut downloader, (_status_state, _speed_state, _speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap())
            .download_connection_count(NonZeroU8::new(3).unwrap()) // 下载连接数
            .build((
                DownloadStatusTrackerExtension { log: true }, // 下载状态追踪扩展
                DownloadSpeedTrackerExtension { log: true }, // 下载速度追踪扩展
                DownloadSpeedLimiterExtension::new(None),
                DownloadBreakpointResumeExtension { // 断点续传扩展
                    download_archiver_builder: BsonFileArchiverBuilder::new(ArchiveFilePath::Suffix("bson".to_string()))
                }
            ));

    // 打印下载进度
    // Print download Progress
    tokio::spawn({
        let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
        let total_size_future = downloader.total_size_future();
        async move {
            let total_len = total_size_future.await;
            if let Some(total_len) = total_len {
                info!("Total size: {:.2} Mb",total_len.get() as f64 / 1024_f64/ 1024_f64);
            }
            while downloaded_len_receiver.changed().await.is_ok() {
                let progress = *downloaded_len_receiver.borrow();
                if let Some(total_len) = total_len {
                    info!("Download Progress: {} %，{}/{}",progress*100/total_len,progress,total_len);
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    tokio::spawn(
        async move {
            let mut futures_unordered = FuturesUnordered::new();
            enum RunFuture {
                DownloadFuture(DownloadFuture),
                Cancel(BoxFuture<'static, ()>),
            }
            enum RunFutureResult {
                DownloadFuture(Result<DownloadingEndCause, DownloadError>),
                Cancel,
            }
            impl Future for RunFuture {
                type Output = RunFutureResult;

                fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                    match self.get_mut() {
                        RunFuture::DownloadFuture(future) => {
                            future.poll_unpin(cx).map(RunFutureResult::DownloadFuture)
                        }
                        RunFuture::Cancel(future) => {
                            future.poll_unpin(cx).map(|_| RunFutureResult::Cancel)
                        }
                    }
                }
            }

            let download_future = downloader.prepare_download()?;
            futures_unordered.push(RunFuture::DownloadFuture(download_future));
            futures_unordered.push(RunFuture::Cancel(async {
                tokio::time::sleep(Duration::from_secs(4)).await
            }.boxed()));
            while let Some(result) = futures_unordered.next().await {
                match result {
                    RunFutureResult::DownloadFuture(r) => {
                        match r? {
                            DownloadingEndCause::DownloadFinished => {
                                break;
                            }
                            DownloadingEndCause::Cancelled => {
                                futures_unordered.push(RunFuture::Cancel(async {
                                    tokio::time::sleep(Duration::from_secs(4)).await
                                }.boxed()));
                                let download_future = downloader.prepare_download()?;
                                futures_unordered.push(RunFuture::DownloadFuture(download_future));
                                continue;
                            }
                        }
                    }
                    RunFutureResult::Cancel => {
                        downloader.cancel().await;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await??;

    info!("DownloadFinished!");

    Ok(())
}
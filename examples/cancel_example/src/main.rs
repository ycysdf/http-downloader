use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tracing::info;
use url::Url;

use http_downloader::{breakpoint_resume::DownloadBreakpointResumeExtension, DownloadingEndCause, HttpDownloaderBuilder, speed_tracker::DownloadSpeedTrackerExtension, status_tracker::DownloadStatusTrackerExtension};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/PCQQ9.7.1/QQ9.7.1.28940.exe")?;
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
            loop {
                let download_future = downloader.prepare_download()?;
                let cancel_future = downloader.cancel();
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    cancel_future.await;
                });

                if matches!(download_future.await?,DownloadingEndCause::DownloadFinished) {
                    break;
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await??;

    info!("DownloadFinished!");

    Ok(())
}
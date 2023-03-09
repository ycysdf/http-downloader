use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::info;
use url::Url;

use http_downloader::{breakpoint_resume::DownloadBreakpointResumeExtension, DownloadingEndCause, HttpDownloaderBuilder, speed_limiter::DownloadSpeedLimiterExtension, speed_tracker::DownloadSpeedTrackerExtension, status_tracker::DownloadStatusTrackerExtension};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("http://mirror.hk.leaseweb.net/speedtest/100mb.bin")?;
    let (downloader, (_status_state, _speed_state, speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap())
            .download_connection_count(NonZeroU8::new(3).unwrap()) // 下载连接数
            .build((
                DownloadStatusTrackerExtension { log: true }, // 下载状态追踪扩展
                DownloadSpeedTrackerExtension { log: true }, // 下载速度追踪扩展
                DownloadSpeedLimiterExtension::new(Some(1024 * 300)),
                DownloadBreakpointResumeExtension { // 断点续传扩展
                    download_archiver_builder: BsonFileArchiverBuilder::new(ArchiveFilePath::Suffix("bson".to_string()))
                }
            ));
    let downloader = Arc::new(downloader);

    // 打印下载进度
    // Print download Progress
    tokio::spawn({
        let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
        let downloader = downloader.clone();
        async move {
            let total_len = downloader.total_size().await;
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

    tokio::spawn({
        let downloader = downloader.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(4)).await;
                info!("Stop!");
                let _ = downloader.cancel().await;
            }
        }
    });
    let r = tokio::spawn({
        let downloader = downloader.clone();
        async move {
            loop {
                info!("Start!");
                let finished_future = downloader.download().await?;
                let dec = finished_future.await?;
                match dec {
                    DownloadingEndCause::DownloadFinished => {
                        return Result::<()>::Ok(());
                    }
                    DownloadingEndCause::Cancelled => {
                        info!("DownloadingEndCause::Cancelled!");
                        continue;
                    }
                }
            }
        }
    });
    r.await??;

    info!("DownloadFinished!");

    Ok(())
}
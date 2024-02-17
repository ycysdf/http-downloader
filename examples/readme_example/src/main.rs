use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tracing::info;
use url::Url;

use http_downloader::{
    breakpoint_resume::DownloadBreakpointResumeExtension,
    HttpDownloaderBuilder,
    speed_tracker::DownloadSpeedTrackerExtension,
    status_tracker::DownloadStatusTrackerExtension,
};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/QQNT/897bf087/QQ9.9.7.21484_x64.exe")?;
    let (mut downloader, (status_state, speed_state, speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap()) // 块大小
            .download_connection_count(NonZeroU8::new(3).unwrap())
            .build((
                // 下载状态追踪扩展
                // by cargo feature "status-tracker" enable
                DownloadStatusTrackerExtension { log: true },
                // 下载速度追踪扩展
                // by cargo feature "speed-tracker" enable
                DownloadSpeedTrackerExtension { log: true },
                // 下载速度限制扩展，
                // by cargo feature "speed-limiter" enable
                DownloadSpeedLimiterExtension::new(None),
                // 断点续传扩展，
                // by cargo feature "breakpoint-resume" enable
                DownloadBreakpointResumeExtension {
                    // BsonFileArchiver by cargo feature "bson-file-archiver" enable
                    download_archiver_builder: BsonFileArchiverBuilder::new(ArchiveFilePath::Suffix("bson".to_string()))
                }
            ));
    info!("Prepare download，准备下载");
    let download_future = downloader.prepare_download()?;

    let _status = status_state.status(); // get download status， 获取状态
    let _status_receiver = status_state.status_receiver; //status watcher，状态监听器
    let _byte_per_second = speed_state.download_speed(); // get download speed，Byte per second，获取速度，字节每秒
    let _speed_receiver = speed_state.receiver; // get download speed watcher，速度监听器

    // downloader.cancel().await; // 取消下载

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

                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    });

    // 下载速度限制
    // Download speed limit
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        info!("Start speed limit，开始限速");
        speed_limiter.change_speed(Some(1024 * 1024 * 2)).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        info!("Remove the download speed limit，解除速度限制");
        speed_limiter.change_speed(None).await;
    });

    info!("Start downloading until the end，开始下载直到结束");
    let dec = download_future.await?;
    info!("Downloading end cause: {:?}", dec);
    Ok(())
}
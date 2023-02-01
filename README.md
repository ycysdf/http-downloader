<div>
  <!-- Crates version -->
  <a href="https://crates.io/crates/http-downloader">
    <img src="https://shields.io/crates/v/http-downloader" alt="Crates.io version" />
  </a>
  <!-- Downloads -->
  <a href="https://crates.io/crates/http-downloader">
    <img src="https://shields.io/crates/d/http-downloader" alt="Download" />
  </a>
  <!-- Downloads -->
  <a href="https://github.com/ycysdf/http-downloader/blob/main/LICENSE">
    <img src="https://shields.io/crates/l/http-downloader" alt="LICENSE" />
  </a>
</div>


<div>
  <h4>
    <a href="https://github.com/ycysdf/http-downloader/blob/main/examples"> Examples </a>
    <span> | </span>
    <a href="https://github.com/ycysdf/http-downloader/blob/main/README.ZH.md"> 中文 </a>
  </h4>
</div>


## Features 

- Multithreaded download
- Breakpoint resume
- Download speed limit
- Download speed tracking
- Add functionality through extensions

## Required minimum dependency

```toml
http-downloader = { version = "0.1" }
url = { version = "2" }
tokio = { version = "1", features = ["rt", "macros"] }
```

## A simple http downloader

terminal ui：[https://github.com/ycysdf/http-downloader-tui](https://github.com/ycysdf/http-downloader-tui)

## Example

```rust
use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tracing::info;
use url::Url;

use http_downloader::{
    breakpoint_resume::DownloadBreakpointResumeExtension,
    HttpDownloaderBuilder,
    speed_limiter::DownloadSpeedLimiterExtension,
    speed_tracker::DownloadSpeedTrackerExtension,
    status_tracker::DownloadStatusTrackerExtension,
};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};




#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/PCQQ9.6.9/QQ9.6.9.28878.exe")?;
    let (downloader, (status_state, speed_state, speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap()) // 块大小
            .download_connection_count(NonZeroU8::new(3).unwrap()) // 下载连接数
            .build((
                // 下载状态追踪扩展
                // by cargo feature "status-tracker" enable
                DownloadStatusTrackerExtension { log: true },
                // 下载速度追踪扩展
                // by cargo feature "speed-tracker" enable
                DownloadSpeedTrackerExtension { log: true },
                // 下载速度限制扩展，
                // by cargo feature "speed-limiter" enable
                DownloadSpeedLimiterExtension {
                    byte_count_per: None
                },
                // 断点续传扩展，
                // by cargo feature "breakpoint-resume" enable
                DownloadBreakpointResumeExtension {
                    // BsonFileArchiver by cargo feature "bson-file-archiver" enable
                    download_archiver_builder: BsonFileArchiverBuilder::new(ArchiveFilePath::Suffix("bson".to_string()))
                }
            ));
    info!("Start download，开始下载");
    let finished_future = downloader.start().await?;

    let _status = status_state.status(); // get download status， 获取状态
    let _status_receiver = status_state.status_receiver; //status watcher，状态监听器
    let _byte_per_second = speed_state.download_speed(); // get download speed，Byte per second，获取速度，字节每秒
    let _speed_receiver = speed_state.receiver; // get download speed watcher，速度监听器

    // downloader.cancel() // 取消下载

    // 打印下载进度
    // Print download Progress
    tokio::spawn({
        let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
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

    info!("Wait for download to end，等待下载结束");
    let dec = finished_future.await?;
    info!("Downloading end cause: {:?}", dec);
    Ok(())
}
```
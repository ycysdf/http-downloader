<div align="center">
  <h4>
    <a href="https://github.com/ycysdf/http-downloader/blob/main/examples"> Examples </a>
    <span> | </span>
    <a href="https://github.com/ycysdf/http-downloader/blob/main/README.md"> English </a>
  </h4>
</div>

## 一个简单的 Http 下载器

## 功能

- 多线程下载
- 断点续传
- 下载速度限制
- 下载速度追踪
- 通过扩展去增加功能

## 示例

```rust
use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tracing::info;
use url::Url;

use http_downloader::{
    breakpoint_resume::{DownloadBreakpointResume, FileSave},
    HttpDownloaderBuilder,
    speed_limiter::DownloadSpeedLimiter,
    speed_tracker::DownloadSpeedTracker,
    status_tracker::DownloadStatusTracker,
};

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }

    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/PCQQ9.6.9/QQ9.6.9.28878.exe")?;
    let (downloader, (_status_state, _speed_state, speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap())
            .download_connection_count(NonZeroU8::new(3).unwrap()) // 下载连接数
            .build((
                DownloadStatusTracker { log: true }, // 下载状态追踪扩展
                DownloadSpeedTracker { log: true }, // 下载速度追踪扩展
                DownloadSpeedLimiter {  // 下载速度限制扩展
                    byte_count_per: None
                },
                DownloadBreakpointResume { // 断点续传扩展
                    file_save: FileSave::Suffix("bson".to_string()),
                }
            ));
    info!("开始下载");
    let finished_future = downloader.start().await?;

    // 打印下载进度
    // Print download Progress
    tokio::spawn({
        let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
        async move {
            let total_len = downloader.total_size().await;
            if let Some(total_len) = total_len {
                info!("总大小: {:.2} Mb",total_len as f64 / 1024_f64/ 1024_f64);
            }
            while downloaded_len_receiver.changed().await.is_ok() {
                let progress = *downloaded_len_receiver.borrow();
                if let Some(total_len) = total_len {
                    info!("下载进度: {} %，{}/{}",progress*100/total_len,progress,total_len);
                }

                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    });

    // 下载速度限制
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        info!("开始限速");
        speed_limiter.change_speed(Some(1024 * 1024 * 2)).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        info!("解除速度限制");
        speed_limiter.change_speed(None).await;
    });

    info!("等待下载结束");
    let dec = finished_future.await?;
    info!("下载结束原因: {:?}", dec);
    Ok(())
}
```
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

这个库是我正在做的一个下载器的 Http 下载部分，目前没有编写单元测试，可能存在一些没发现bug，欢迎贡献代码

> 项目正在准备进行重构，将不依赖异步运行时

## 功能：

- 多线程下载
- 断点续传
- 下载速度限制
- 下载速度追踪
- 在下载时修改
    - 下载并行连接数
    - 速度限制
    - 下载块大小

## cargo futures

一些功能默认没有开启，如需开启，请设置 cargo features

```toml
[features]
# 默认开启 tokio tracing
default = ["tracing"]
# 一些类型作为 async-graphql 输入或者输出对象
async-graphql = ["dep:async-graphql"]
# 全部扩展
all-extensions = ["status-tracker", "speed-limiter", "speed-tracker", "breakpoint-resume", "tracing", "bson-file-archiver"]
# 下载状态追踪
status-tracker = ["tracing"]
# 下载速度追踪
speed-tracker = ["tracing"]
# 下载速度限制
speed-limiter = ["tracing"]
# 断点续传
breakpoint-resume = ["tracing"]
# 断点续传，文件存储器
bson-file-archiver = ["breakpoint-resume", "tracing", "serde", "bson", "url/serde"]
```

## 最少需要添加以下依赖

```toml
http-downloader = { version = "0.1" }
url = { version = "2" }
tokio = { version = "1", features = ["rt", "macros"] }
```

## 终端 UI

使用此库做的一个，简单的终端
UI ：[https://github.com/ycysdf/http-downloader-tui](https://github.com/ycysdf/http-downloader-tui)

## 用例

通过 `HttpDownloaderBuilder` `build` 函数参数去设置需要添加的扩展，需要传入一个元组，元组的成员就是扩展

在下面实例里，我传入了4 个扩展，所以 `build` 函数除了返回 下载器实例以外，还会返回一个4个成员的元组，这个元组包含了扩展的状态信息，

例如 `DownloadSpeedTrackerExtension` 扩展，就对应 `DownloadSpeedTrackerState` 状态

通过`DownloadSpeedTrackerState` 的  `reciver` 成员就可以去监听下载速度，或者通过 `download_speed`函数直接去获取下载速度

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
    let test_url = Url::parse("https://releases.ubuntu.com/22.04/ubuntu-22.04.2-desktop-amd64.iso")?;
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
    let download_future = downloader.prepare_download().await?;

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

    info!("Start downloading until the end，开始下载直到结束");
    let dec = download_future.await?;
    info!("Downloading end cause: {:?}", dec);
    Ok(())
}
```

更多用例，请看这里：[Examples](https://github.com/ycysdf/http-downloader/blob/main/examples)
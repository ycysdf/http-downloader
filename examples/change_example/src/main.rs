use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{pin_mut, StreamExt};
use url::Url;

use http_downloader::HttpDownloaderBuilder;
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;
use http_downloader::speed_tracker::DownloadSpeedTrackerExtension;

#[tokio::main]
async fn main() -> Result<()> {
    {
        tracing_subscriber::fmt::init();
    }
    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/QQNT/897bf087/QQ9.9.7.21484_x64.exe")?;
    let (mut downloader, (_, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 4).unwrap())
            .download_connection_count(NonZeroU8::new(3).unwrap()) // 下载连接数
            .build((DownloadSpeedLimiterExtension::new(None), DownloadSpeedTrackerExtension {
                log: true
            }));

    let download_future = downloader.prepare_download()?;
    let downloader = Arc::new(downloader);

    tokio::spawn({
        let downloader = downloader.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                tracing::info!("Change connection count to 1");
                downloader.change_connection_count(NonZeroU8::new(1).unwrap()).unwrap();
                tokio::time::sleep(Duration::from_secs(2)).await;
                tracing::info!("Change connection count to 8");
                downloader.change_connection_count(NonZeroU8::new(8).unwrap()).unwrap();
                tokio::time::sleep(Duration::from_secs(2)).await;
                tracing::info!("Change connection count to 4");
                downloader.change_connection_count(NonZeroU8::new(4).unwrap()).unwrap();
            }
        }
    });
    tokio::spawn({
        let downloader = downloader.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(4)).await;
                tracing::info!("change_chunk_size to 1M");
                downloader.change_chunk_size(NonZeroUsize::new(1024 * 1024 * 1).unwrap()).unwrap();
                tokio::time::sleep(Duration::from_secs(4)).await;
                tracing::info!("change_chunk_size to 8M");
                downloader.change_chunk_size(NonZeroUsize::new(1024 * 1024 * 8).unwrap()).unwrap();
                tokio::time::sleep(Duration::from_secs(4)).await;
                tracing::info!("change_chunk_size to 4M");
                downloader.change_chunk_size(NonZeroUsize::new(1024 * 1024 * 4).unwrap()).unwrap();
            }
        }
    });
    tokio::spawn({
        let downloader = downloader.clone();
        async move {
            // 等待大小信息获取完毕
            let _ = downloader.total_size_future().await;
            let chunks_stream = downloader.chunks_stream().unwrap();
            pin_mut!(chunks_stream);
            while let Some(item) = chunks_stream.next().await {
                println!("chunk count :{}", item.len());
            }
        }
    });

    let dec = download_future.await?;
    tracing::info!("Downloading end cause: {:?}", dec);
    Ok(())
}
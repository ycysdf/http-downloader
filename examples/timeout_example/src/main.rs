use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use url::Url;

use http_downloader::HttpDownloaderBuilder;

#[tokio::main]
async fn main() -> Result<()> {
    let save_dir = PathBuf::from("C:/download");
    let test_url = Url::parse("https://dldir1.qq.com/qqfile/qq/PCQQ9.6.9/QQ9.6.9.28878.exe")?;
    let (downloader, ()) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .timeout(Some(Duration::from_millis(1)))
            .build(());
    let finished_future = downloader.start().await?;
    let dec = finished_future.await?;
    Ok(())
}
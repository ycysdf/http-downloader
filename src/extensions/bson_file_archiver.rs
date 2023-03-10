use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Error;
use async_trait::async_trait;

use crate::{DownloadArchiveData, DownloadingState, HttpDownloadConfig};
use crate::breakpoint_resume::{DownloadDataArchiver, DownloadDataArchiverBuilder, DownloadEndInfo};

pub enum ArchiveFilePath<T: Display> {
    Absolute(PathBuf),
    Suffix(T),
}

impl<T: Display> ArchiveFilePath<T> {
    pub fn get_file_path(self, origin_file: &Path) -> PathBuf {
        match self {
            ArchiveFilePath::Absolute(path) => { path }
            ArchiveFilePath::Suffix(suffix) => { origin_file.with_extension(OsStr::new(&format!("{}.{}", origin_file.extension().and_then(|n| n.to_str()).unwrap_or(""), suffix))) }
        }
    }
}

pub struct BsonFileArchiverBuilder<T: Display> {
    archive_file_path: ArchiveFilePath<T>,
}

impl<T: Display> BsonFileArchiverBuilder<T> {
    pub fn new(archive_file_path: ArchiveFilePath<T>) -> Self {
        Self {
            archive_file_path
        }
    }
}

impl<T: Display> DownloadDataArchiverBuilder for BsonFileArchiverBuilder<T> {
    type DownloadDataArchiver = BsonFileArchiver;

    fn build(self, config: &HttpDownloadConfig) -> Self::DownloadDataArchiver {
        BsonFileArchiver {
            archive_file_path: self.archive_file_path.get_file_path(&config.file_path())
        }
    }
}

pub struct BsonFileArchiver {
    pub archive_file_path: PathBuf,
}

#[async_trait]
impl DownloadDataArchiver for BsonFileArchiver {
    async fn save(&self, data: Box<DownloadArchiveData>) -> Result<(), anyhow::Error> {
        let bytes = bson::to_vec(&data)?;
        tokio::fs::write(&self.archive_file_path, bytes).await.map_err(|err| {
            err
        })?;
        Ok(())
    }

    async fn load(&self) -> anyhow::Result<Option<Box<DownloadArchiveData>>, Error> {
        if !self.archive_file_path.exists() {
            return Ok(None);
        }
        let bytes = tokio::fs::read(&self.archive_file_path).await?;
        let data = bson::from_slice::<DownloadArchiveData>(&bytes)?;
        Ok(Some(Box::new(data)))
    }

    async fn download_started(&self, _downloading_state: &Arc<DownloadingState>, _is_resume: bool) -> anyhow::Result<(), Error> {
        Ok(())
    }

    async fn download_ended<'a>(&'a self, end_info: DownloadEndInfo<'a>) {
        match end_info {
            DownloadEndInfo::StartError(_) => {}
            DownloadEndInfo::DownloadEnd(result) => {
                if result.is_err() {
                    let _ = tokio::fs::remove_file(&self.archive_file_path).await;
                }
            }
        }
    }
}

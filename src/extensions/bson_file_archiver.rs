use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use anyhow::Error;
use async_trait::async_trait;

use crate::{DownloadArchiveData, HttpDownloadConfig};
use crate::breakpoint_resume::{DownloadDataArchiver, DownloadDataArchiverBuilder};

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
    async fn save(&self, data: Box<DownloadArchiveData>) -> anyhow::Result<(), Error> {
        let bytes = bson::to_vec(&data);
        let bytes = bytes.map_err(|err| {
            tracing::error!("serialize archive data failed! {}", err);
            Error::new(err)
        })?;
        Ok(tokio::fs::write(&self.archive_file_path, bytes).await.map_err(|err| {
            tracing::error!("write data to file failed! {}", err);
            err
        })?)
    }

    async fn load(&self) -> anyhow::Result<Option<DownloadArchiveData>, Error> {
        if !self.archive_file_path.exists() {
            return Ok(None);
        }
        let bytes = tokio::fs::read(&self.archive_file_path).await.map_err(|err| {
            tracing::error!("read file {:?} failed! {}", &self.archive_file_path, err);
            err
        })?;
        let data = bson::from_slice::<DownloadArchiveData>(&bytes).map_err(|err| {
            tracing::error!("deserialize file {:?} failed! {}", &self.archive_file_path, err);
            err
        })?;
        Ok(Some(data))
    }

    async fn download_finished(&self) {
        let _ = tokio::fs::remove_file(&self.archive_file_path).await;
    }
}

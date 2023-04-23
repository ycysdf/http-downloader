use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use anyhow::Error;
use futures_util::future::{BoxFuture};
use futures_util::FutureExt;

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

impl DownloadDataArchiver for BsonFileArchiver {
    fn save(&self, data: Box<DownloadArchiveData>) -> BoxFuture<'static,Result<(), anyhow::Error>> {
        let archive_file_path = self.archive_file_path.clone();
        async move {
            let bytes = bson::to_vec(&data)?;
            tokio::fs::write(&archive_file_path, bytes).await.map_err(|err| {
                err
            })?;
            Ok(())
        }.boxed()
    }

    fn load(&self) -> BoxFuture<'static,anyhow::Result<Option<Box<DownloadArchiveData>>, Error>> {
        let archive_file_path = self.archive_file_path.clone();
        async move{
            if !archive_file_path.exists() {
                return Ok(None);
            }
            let bytes = tokio::fs::read(&archive_file_path).await?;
            let data = bson::from_slice::<DownloadArchiveData>(&bytes)?;
            Ok(Some(Box::new(data)))
        }.boxed()
    }
}

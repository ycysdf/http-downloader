use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::{ChunkData, DownloadError, DownloadingEndCause, DownloadStartError, DownloadStopError, HttpFileDownloader};

#[cfg(feature = "breakpoint-resume")]
pub mod breakpoint_resume;
#[cfg(feature = "bson-file-archiver")]
pub mod bson_file_archiver;
#[cfg(feature = "speed-limiter")]
pub mod speed_limiter;
#[cfg(feature = "speed-tracker")]
pub mod speed_tracker;
#[cfg(feature = "status-tracker")]
pub mod status_tracker;

pub type DownloadFuture = BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>;
pub type DownloadCancelFuture<'a> = BoxFuture<'a, Result<(), DownloadStopError>>;


#[async_trait]
pub trait DownloaderWrapper: Send + Sync + 'static {
    async fn prepare_download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
    ) -> Result<(), DownloadStartError> {
        Ok(())
    }
    async fn handle_prepare_download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
        prepare_download_result: Result<DownloadFuture, DownloadStartError>,
    ) -> Result<DownloadFuture, DownloadStartError> {
        prepare_download_result
    }
    async fn download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError> {
        Ok(download_future)
    }
    async fn cancel(&self, cancel_future: DownloadCancelFuture<'_>) -> Result<(), DownloadStopError> {
        cancel_future.await
    }
}

pub trait DownloadExtensionInstance: DownloaderWrapper + Send + Sync + 'static {
    type ExtensionParam;
    type ExtensionState;

    fn new(param: Self::ExtensionParam, downloader: &mut HttpFileDownloader) -> (Self, Self::ExtensionState) where Self: Sized;
}

impl DownloaderWrapper for () {}

impl DownloadExtensionInstance for () {
    type ExtensionParam = ();
    type ExtensionState = ();

    fn new(_param: Self::ExtensionParam, _: &mut HttpFileDownloader) -> (Self, Self::ExtensionState) {
        ((), ())
    }
}


macro_rules! impl_download_extension_tuple {
    (
        $(($de:ident,$ds:ident)),*
    ) => {
        #[async_trait]
        #[allow(non_snake_case)]
        impl<
            $($de: DownloadExtensionInstance,)*
            > DownloaderWrapper for ($($de,)*)
        {
            async fn download(
                &mut self,
                downloader: &mut HttpFileDownloader,
                download_future: DownloadFuture,
            ) -> Result<DownloadFuture, DownloadStartError> {
                let ($($de,)*) = &mut self;
                $(let download_future = $de.download(downloader,download_future).await?;)*
                Ok(download_future)
            }
            async fn cancel(&self, cancel_future: DownloadCancelFuture<'_>) -> Result<(), DownloadStopError> {
                let ($($de,)*) = &self;
                $(let cancel_future = $de.cancel(cancel_future);)*
                Ok(cancel_future.await?)
            }
        }

        #[allow(non_snake_case)]
        impl<
            $($de: DownloadExtensionInstance,)*
            > DownloadExtensionInstance for ($($de,)*)
        {
            type ExtensionState = ($($de::ExtensionState,)*);
            type ExtensionParam = ($($de::ExtensionParam,)*);

            fn new(param:Self::ExtensionParam,downloader: &mut HttpFileDownloader) -> (Self, Self::ExtensionState) {
                let ($($ds,)*) = param;
                $(let ($de, $ds) = $de::new($ds,downloader);)*
                (($($de,)*), ($($ds,)*))
            }
        }

    };
}

impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3));

#[cfg_attr(
feature = "async-graphql",
derive(async_graphql::SimpleObject),
graphql(complex)
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct DownloadArchiveData {
    pub downloaded_len: u64,
    pub downloading_duration: u32,
    pub chunk_data: Option<ChunkData>,
}

#[cfg(feature = "async-graphql")]
#[cfg_attr(feature = "async-graphql", async_graphql::ComplexObject)]
impl DownloadArchiveData {
    #[cfg(feature = "async-graphql")]
    pub async fn average_download_speed(&self) -> u64 {
        self.get_average_download_speed()
    }
}

impl DownloadArchiveData {
    pub fn get_average_download_speed(&self) -> u64 {
        if self.downloading_duration == 0 {
            return 0;
        }
        self.downloaded_len / self.downloading_duration as u64
    }
}/*

#[derive(Default)]
pub struct DownloadContext {
    pub downloading_state_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadingState>>>,
    pub downloaded_len_change_notify: Option<Arc<dyn DownloadedLenChangeNotify>>,
    pub archive_data: Option<Box<DownloadArchiveData>>,
    pub breakpoint_resume: bool,
}

impl DownloadContext {
    pub fn new() -> Self {
        Default::default()
    }
}*/

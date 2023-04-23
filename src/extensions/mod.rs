use anyhow::Result;
use futures_util::future::{BoxFuture};
use futures_util::FutureExt;

use crate::{ChunkData, DownloadError, DownloadingEndCause, DownloadStartError, HttpFileDownloader};

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


pub trait DownloaderWrapper: Send+Sync+'static {
    fn prepare_download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
    ) -> Result<(), DownloadStartError> {
        Ok(())
    }
    fn handle_prepare_download_result(
        &mut self,
        _downloader: &mut HttpFileDownloader,
        prepare_download_result: Result<DownloadFuture, DownloadStartError>,
    ) -> Result<DownloadFuture, DownloadStartError> {
        prepare_download_result
    }
    fn download(
        &mut self,
        _downloader: &mut HttpFileDownloader,
        download_future: DownloadFuture,
    ) -> Result<DownloadFuture, DownloadStartError> {
        Ok(download_future)
    }
    fn on_cancel(&self)-> BoxFuture<'static,()> {
        futures_util::future::ready(()).boxed()
    }
}

pub trait DownloadExtensionBuilder: 'static {
    type Wrapper: DownloaderWrapper;
    type ExtensionState;

    fn build(self, downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) where Self: Sized;
}

impl DownloaderWrapper for () {}

impl DownloadExtensionBuilder for () {
    type Wrapper = ();
    type ExtensionState = ();

    fn build(self, _downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) where Self: Sized {
        ((), ())
    }
}


macro_rules! impl_download_extension_tuple {
    (
        $(($de:ident,$ds:ident)),*
    ) => {
        #[allow(non_snake_case)]
        impl<
            $($de: DownloaderWrapper,)*
            > DownloaderWrapper for ($($de,)*)
        {
            fn prepare_download(
                &mut self,
                downloader: &mut HttpFileDownloader,
            ) -> Result<(), DownloadStartError> {
                let ($($de,)*) = self;
                $($de.prepare_download(downloader)?;)*
                Ok(())
            }
            fn handle_prepare_download_result(
                &mut self,
                downloader: &mut HttpFileDownloader,
                prepare_download_result: Result<DownloadFuture, DownloadStartError>,
            ) -> Result<DownloadFuture, DownloadStartError> {
                let ($($de,)*) = self;
                $(let prepare_download_result = $de.handle_prepare_download_result(downloader,prepare_download_result);)*
                prepare_download_result
            }
            fn download(
                &mut self,
                downloader: &mut HttpFileDownloader,
                download_future: DownloadFuture,
            ) -> Result<DownloadFuture, DownloadStartError> {
                let ($($de,)*) = self;
                $(let download_future = $de.download(downloader,download_future)?;)*
                Ok(download_future)
            }
            fn on_cancel(&self) -> BoxFuture<'static,()> {
                let ($($de,)*) = &self;
                $(let $de = $de.on_cancel();)*
                async move{
                    $($de.await;)*
                }.boxed()
            }
        }

        #[allow(non_snake_case)]
        impl<
            $($de: DownloadExtensionBuilder,)*
            > DownloadExtensionBuilder for ($($de,)*)
        {
            type Wrapper = ($($de::Wrapper,)*);
            type ExtensionState = ($($de::ExtensionState,)*);

            fn build(self,downloader: &mut HttpFileDownloader) -> (Self::Wrapper, Self::ExtensionState) {
                let ($($ds,)*) = self;
                $(let ($de, $ds) = $ds.build(downloader);)*
                (($($de,)*), ($($ds,)*))
            }
        }

    };
}

impl_download_extension_tuple!((DE1,ds1));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3),(DE4,ds4));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3),(DE4,ds4),(DE5,ds5));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3),(DE4,ds4),(DE5,ds5),(DE6,ds6));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3),(DE4,ds4),(DE5,ds5),(DE6,ds6),(DE7,ds7));
impl_download_extension_tuple!((DE1,ds1),(DE2,ds2),(DE3,ds3),(DE4,ds4),(DE5,ds5),(DE6,ds6),(DE7,ds7),(DE8,ds8));

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
}
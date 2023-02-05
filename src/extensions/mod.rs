use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tokio::sync;

use crate::{ChunkData, DownloadedLenChangeNotify, DownloadError, DownloadingEndCause, DownloadStartError, DownloadStopError, DownloadWay, HttpFileDownloader};

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

#[async_trait]
pub trait DownloadController: Send + Sync + 'static {
    async fn download(
        self: Arc<Self>,
        params: DownloadParams,
    ) -> Result<BoxFuture<'static, Result<DownloadingEndCause, DownloadError>>, DownloadStartError>;
    async fn cancel(&self) -> Result<(), DownloadStopError>;
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct DownloadArchiveData {
    pub downloaded_len: u64,
    pub downloading_duration: u32,
    pub chunk_data: Option<ChunkData>,
}

#[derive(Default)]
pub struct DownloadParams {
    pub download_way_oneshot_vec: Vec<sync::oneshot::Sender<Arc<DownloadWay>>>,
    pub downloaded_len_change_notify: Option<Arc<dyn DownloadedLenChangeNotify>>,
    pub archive_data: Option<Box<DownloadArchiveData>>,
    pub breakpoint_resume: bool,
}

impl DownloadParams {
    pub fn new() -> Self {
        Default::default()
    }
}

pub trait DownloadExtension<DC: DownloadController> {
    type DownloadController;
    type ExtensionState;
    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<DC>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState);
}

impl DownloadExtension<HttpFileDownloader> for () {
    type DownloadController = HttpFileDownloader;
    type ExtensionState = ();

    fn layer(
        self,
        downloader: Arc<HttpFileDownloader>,
        inner: Arc<HttpFileDownloader>,
    ) -> (Arc<Self::DownloadController>, Self::ExtensionState) {
        drop(downloader);
        (inner, ())
    }
}

macro_rules! impl_download_extension_tuple {
    (
        $start:ident,$(($dc:ident,$de:ident,$der:ident)),*,$end:ident
    ) => {
        #[allow(non_snake_case)]
        impl<
            $($dc: DownloadController,)*
            $end: DownloadController,
            $($de: DownloadExtension<$dc, DownloadController = $der>),*
            > DownloadExtension<$start> for ($($de,)*)
        {
            type DownloadController = $end;
            type ExtensionState = ($($de::ExtensionState,)*);

            fn layer(
                self,
                downloader: Arc<HttpFileDownloader>,
                inner: Arc<$start>,
            ) -> (
                Arc<Self::DownloadController>,
                Self::ExtensionState,
            ) {
                let ($($de,)*) = self;
                $(let (inner, $dc) = $de.layer(downloader.clone(), inner);)*
                (inner, ($($dc,)*))
            }
        }

    };
}

impl_download_extension_tuple!(DC1, (DC1, DE1, DC2), DC2);
impl_download_extension_tuple!(DC1, (DC1, DE1, DC2), (DC2, DE2, DC3), DC3);
impl_download_extension_tuple!(DC1, (DC1, DE1, DC2), (DC2, DE2, DC3), (DC3, DE3, DC4), DC4);

impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    DC5
);
impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    (DC5, DE5, DC6),
    DC6
);
impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    (DC5, DE5, DC6),
    (DC6, DE6, DC7),
    DC7
);
impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    (DC5, DE5, DC6),
    (DC6, DE6, DC7),
    (DC7, DE7, DC8),
    DC8
);

impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    (DC5, DE5, DC6),
    (DC6, DE6, DC7),
    (DC7, DE7, DC8),
    (DC8, DE8, DC9),
    DC9
);

impl_download_extension_tuple!(
    DC1,
    (DC1, DE1, DC2),
    (DC2, DE2, DC3),
    (DC3, DE3, DC4),
    (DC4, DE4, DC5),
    (DC5, DE5, DC6),
    (DC6, DE6, DC7),
    (DC7, DE7, DC8),
    (DC8, DE8, DC9),
    (DC9, DE9, DC10),
    DC10
);

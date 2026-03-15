use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};

use async_trait::async_trait;

use crate::arxiv::{Metadata, PaperSource, PaperSourceError};
use crate::cache::MkCache;
use crate::convert::Converter;
use crate::disk_cache::DiskCache;

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<Mutex<MkCache>>,
    pub arxiv_client: Arc<dyn PaperSource + Send + Sync>,
    pub biorxiv_client: Arc<dyn PaperSource + Send + Sync>,
    pub converter: Arc<dyn Converter + Send + Sync>,
    pub disk: Option<Arc<DiskCache>>,
    pub convert_limit: Arc<Semaphore>,
}

impl AppState {
    pub fn new<C, V>(cap: usize, client: C, converter: V, disk: Option<Arc<DiskCache>>) -> Self
    where
        C: PaperSource + Send + Sync + 'static,
        V: Converter + Send + Sync + 'static,
    {
        Self::new_with_clients(cap, client, NullPaperSourceClient, converter, disk)
    }

    pub fn new_with_clients<A, B, V>(
        cap: usize,
        arxiv_client: A,
        biorxiv_client: B,
        converter: V,
        disk: Option<Arc<DiskCache>>,
    ) -> Self
    where
        A: PaperSource + Send + Sync + 'static,
        B: PaperSource + Send + Sync + 'static,
        V: Converter + Send + Sync + 'static,
    {
        let permits = num_cpus::get().max(1);
        Self {
            cache: Arc::new(Mutex::new(MkCache::new(cap))),
            arxiv_client: Arc::new(arxiv_client),
            biorxiv_client: Arc::new(biorxiv_client),
            converter: Arc::new(converter),
            disk,
            convert_limit: Arc::new(Semaphore::new(permits)),
        }
    }
}

struct NullPaperSourceClient;

#[async_trait]
impl PaperSource for NullPaperSourceClient {
    async fn exists(&self, _id: &str) -> Result<bool, PaperSourceError> {
        Ok(false)
    }

    async fn get_source_archive(&self, _id: &str) -> Result<bytes::Bytes, PaperSourceError> {
        Err(PaperSourceError::NotImplemented)
    }

    async fn get_pdf(&self, _id: &str) -> Result<bytes::Bytes, PaperSourceError> {
        Err(PaperSourceError::NotImplemented)
    }

    async fn get_metadata(&self, _id: &str) -> Result<Metadata, PaperSourceError> {
        Err(PaperSourceError::NotImplemented)
    }
}

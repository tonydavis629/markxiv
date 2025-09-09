use std::sync::Arc;

use axum::extract::FromRef;
use tokio::sync::Mutex;

use crate::arxiv::ArxivClient;
use crate::cache::MkCache;
use crate::convert::Converter;
use crate::disk_cache::{DiskCache, DiskCacheConfig};

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<Mutex<MkCache>>,
    pub client: Arc<dyn ArxivClient + Send + Sync>,
    pub converter: Arc<dyn Converter + Send + Sync>,
    pub disk: Option<Arc<DiskCache>>,
}

impl AppState {
    pub fn new<C, V>(cap: usize, client: C, converter: V, disk: Option<Arc<DiskCache>>) -> Self
    where
        C: ArxivClient + Send + Sync + 'static,
        V: Converter + Send + Sync + 'static,
    {
        Self {
            cache: Arc::new(Mutex::new(MkCache::new(cap))),
            client: Arc::new(client),
            converter: Arc::new(converter),
            disk,
        }
    }
}

impl FromRef<AppState> for Arc<Mutex<MkCache>> {
    fn from_ref(input: &AppState) -> Self {
        input.cache.clone()
    }
}

impl FromRef<AppState> for Arc<dyn ArxivClient + Send + Sync> {
    fn from_ref(input: &AppState) -> Self {
        input.client.clone()
    }
}

impl FromRef<AppState> for Arc<dyn Converter + Send + Sync> {
    fn from_ref(input: &AppState) -> Self {
        input.converter.clone()
    }
}

impl FromRef<AppState> for Option<Arc<DiskCache>> {
    fn from_ref(input: &AppState) -> Self {
        input.disk.clone()
    }
}

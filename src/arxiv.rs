use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum ArxivError {
    #[error("not found")]
    NotFound,
    #[error("pdf only")]
    PdfOnly,
    #[error("network error: {0}")]
    Network(String),
    #[error("not implemented")]
    NotImplemented,
}

#[async_trait]
pub trait ArxivClient {
    async fn exists(&self, id: &str) -> Result<bool, ArxivError>;
    async fn get_source_archive(&self, id: &str) -> Result<Bytes, ArxivError>;
}

pub struct ReqwestArxivClient;

impl ReqwestArxivClient {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ArxivClient for ReqwestArxivClient {
    async fn exists(&self, _id: &str) -> Result<bool, ArxivError> {
        // TODO: Implement using export.arxiv.org API; stubbed for now
        Err(ArxivError::NotImplemented)
    }

    async fn get_source_archive(&self, _id: &str) -> Result<Bytes, ArxivError> {
        // TODO: Implement download of e-print source; stubbed for now
        Err(ArxivError::NotImplemented)
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct MockArxivClient {
        pub exists_response: Result<bool, ArxivError>,
        pub archive_response: Result<Bytes, ArxivError>,
        pub exists_calls: Arc<AtomicUsize>,
        pub archive_calls: Arc<AtomicUsize>,
    }

    impl MockArxivClient {
        pub fn new(exists_response: Result<bool, ArxivError>, archive_response: Result<Bytes, ArxivError>) -> Self {
            Self {
                exists_response,
                archive_response,
                exists_calls: Arc::new(AtomicUsize::new(0)),
                archive_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl ArxivClient for MockArxivClient {
        async fn exists(&self, _id: &str) -> Result<bool, ArxivError> {
            self.exists_calls.fetch_add(1, Ordering::SeqCst);
            self.exists_response.clone()
        }

        async fn get_source_archive(&self, _id: &str) -> Result<Bytes, ArxivError> {
            self.archive_calls.fetch_add(1, Ordering::SeqCst);
            self.archive_response.clone()
        }
    }
}

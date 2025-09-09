use async_trait::async_trait;
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum ConvertError {
    #[error("conversion failed: {0}")]
    Failed(String),
    #[error("not implemented")]
    NotImplemented,
}

#[async_trait]
pub trait Converter {
    async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError>;
}

pub struct PandocConverter;

impl PandocConverter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Converter for PandocConverter {
    async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError> {
        // TODO: Implement using `tar` and `pandoc` subprocesses
        Err(ConvertError::NotImplemented)
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;

    pub struct MockConverter {
        pub result: Result<String, ConvertError>,
    }

    #[async_trait]
    impl Converter for MockConverter {
        async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError> {
            self.result.clone()
        }
    }
}

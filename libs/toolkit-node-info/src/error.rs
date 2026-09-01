/// Errors for node information collection
#[derive(Debug, thiserror::Error)]
pub enum NodeInfoError {
    #[error("System information collection failed: {0}")]
    SysInfoCollectionFailed(String),

    #[error("System capabilities collection failed: {0}")]
    SysCapCollectionFailed(String),

    #[error("Failed to get hardware UUID: {0}")]
    HardwareUuidFailed(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<anyhow::Error> for NodeInfoError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

use thiserror::Error;

#[derive(Error, Debug)]
pub enum RudyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid disk geometry: total sectors ({total_sectors}) too small for Rudy layout (minimum required: {min_required})")]
    DiskTooSmall {
        total_sectors: u64,
        min_required: u64,
    },

    #[error("Target disk {dev_path} is not a valid Rudy installation: {reason}")]
    NotRudyDisk { dev_path: String, reason: String },

    #[error("Asset error: {0}")]
    Asset(String),

    #[error("Partition table error: {0}")]
    Partition(String),

    #[error("Filesystem formatting error: {0}")]
    Format(String),

    #[error("Validation error: {0}")]
    Validation(String),
}

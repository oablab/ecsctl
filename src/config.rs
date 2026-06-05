use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ~/.ecsctl/config.toml
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// S3 bucket for staging file transfers
    pub bucket: Option<String>,
    /// Presigned URL expiry in seconds (default: 60)
    pub presign_expiry: Option<u64>,
    /// Default cluster name
    pub cluster: Option<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ecsctl")
            .join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn presign_expiry_secs(&self) -> u64 {
        self.presign_expiry.unwrap_or(60)
    }
}

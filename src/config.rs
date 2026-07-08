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
    /// Aliases: name -> "cluster/service/container[/task_id]"
    #[serde(default)]
    pub aliases: std::collections::HashMap<String, String>,
    /// Groups: name -> list of alias names for batch operations
    #[serde(default)]
    pub groups: std::collections::HashMap<String, Vec<String>>,
    /// Scheduler configuration
    #[serde(default)]
    pub scheduler: Option<SchedulerConfig>,
}

/// [scheduler] section in config.toml
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// IAM role ARN for EventBridge Scheduler execution
    pub role_arn: Option<String>,
    /// Schedule group name (default: "ecsctl-schedules")
    pub group_name: Option<String>,
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

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Resolve a target that may be an @group or a single alias.
    /// Returns a list of alias names.
    pub fn resolve_targets(&self, target: &str) -> Vec<String> {
        if let Some(group_name) = target.strip_prefix('@') {
            self.groups.get(group_name).cloned().unwrap_or_default()
        } else {
            vec![target.to_string()]
        }
    }

    /// Get the scheduler execution role ARN from config, if set.
    pub fn scheduler_role_arn(&self) -> Option<&str> {
        self.scheduler.as_ref().and_then(|s| s.role_arn.as_deref())
    }
}

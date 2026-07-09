use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ~/.ecsctl/config.toml
#[derive(Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
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
#[non_exhaustive]
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
        Self::load_from(&Self::path())
    }

    /// Load config from a custom path. Returns default config if file doesn't exist.
    ///
    /// Use this when consuming ecsctl as a library with a different config location
    /// (e.g. `~/.oabctl/config.toml`).
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
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

    /// Resolve the scheduler role ARN from flag override or config.
    ///
    /// Priority: flag value > config.toml `[scheduler].role_arn` > error.
    /// This keeps the resolution logic testable outside the CLI layer.
    pub fn resolve_scheduler_role_arn(&self, flag_value: Option<String>) -> anyhow::Result<String> {
        flag_value
            .or_else(|| self.scheduler_role_arn().map(|s| s.to_string()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler role ARN required.\n\
                     Provide --role-arn or set [scheduler].role_arn in ~/.ecsctl/config.toml\n\n\
                     Example config:\n  [scheduler]\n  role_arn = \"arn:aws:iam::123456789012:role/ecsctl-scheduler-role\""
                )
            })
    }

    /// Get the scheduler group name from config, or default to "ecsctl-schedules".
    pub fn scheduler_group_name(&self) -> &str {
        self.scheduler
            .as_ref()
            .and_then(|s| s.group_name.as_deref())
            .unwrap_or("ecsctl-schedules")
    }

    /// Resolve an alias to (cluster, service) tuple.
    ///
    /// Note: does NOT expand `@group` prefixes — call [`resolve_targets`](Self::resolve_targets)
    /// first to expand groups into individual alias names, then call this for each.
    pub fn resolve_alias(&self, alias: &str) -> anyhow::Result<(&str, &str)> {
        let target = self
            .aliases
            .get(alias)
            .ok_or_else(|| anyhow::anyhow!("alias '{alias}' not found"))?;

        let parts: Vec<&str> = target.splitn(4, '/').collect();
        match parts.len() {
            2..=4 => Ok((parts[0], parts[1])),
            _ => anyhow::bail!(
                "invalid alias target for '{alias}': expected 'cluster/service', got '{target}'"
            ),
        }
    }
}

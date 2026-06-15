use anyhow::{Context, Result};

use std::process::Command as ProcessCommand;

/// Parse "cluster/task/container" into parts
fn parse_target(s: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() == 3 {
        Ok((parts[0], parts[1], parts[2]))
    } else {
        anyhow::bail!("target must be cluster/task/container")
    }
}

pub async fn run(
    _config: &aws_config::SdkConfig,
    target: &str,
    command: Option<&str>,
) -> Result<()> {
    let (cluster, task, container) = parse_target(target)?;
    let cmd = command.unwrap_or("/bin/sh");

    // ECS Exec only supports interactive mode — always shell out to aws CLI
    // (SSM plugin handles the WebSocket session)
    let status = ProcessCommand::new("aws")
        .args([
            "ecs",
            "execute-command",
            "--cluster",
            cluster,
            "--task",
            task,
            "--container",
            container,
            "--interactive",
            "--command",
            cmd,
        ])
        .status()
        .context("failed to run aws ecs execute-command")?;

    if !status.success() {
        anyhow::bail!("exec exited with status {}", status);
    }

    Ok(())
}

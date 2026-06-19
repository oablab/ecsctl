use anyhow::{Context, Result};
use std::process::Command as ProcessCommand;

/// Parse "cluster/task/container" into parts
pub fn parse_target(s: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() == 3 {
        Ok((parts[0], parts[1], parts[2]))
    } else {
        anyhow::bail!("target must be cluster/task/container")
    }
}

/// Execute a command inside an ECS container.
///
/// Interactive mode requires shelling out to `aws` CLI (SSM plugin handles WebSocket).
/// This is the only operation that cannot be done purely via SDK.
pub async fn run(
    _config: &aws_config::SdkConfig,
    target: &str,
    command: Option<&str>,
) -> Result<()> {
    let (cluster, task, container) = parse_target(target)?;
    let cmd = command.unwrap_or("/bin/sh");
    interactive_exec(cluster, task, container, cmd)
}

/// Shell out to `aws ecs execute-command` for interactive sessions.
/// This is required because the SSM session plugin manages the TTY/WebSocket.
pub fn interactive_exec(cluster: &str, task: &str, container: &str, cmd: &str) -> Result<()> {
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

/// Non-interactive exec via shell-out. Used by cp/sync to run commands inside containers.
/// Returns Ok(()) on success, Err on non-zero exit.
pub fn non_interactive_exec(cluster: &str, task: &str, container: &str, cmd: &str) -> Result<()> {
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
        anyhow::bail!("ecs exec failed with status {}", status);
    }
    Ok(())
}

use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
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
    config: &aws_config::SdkConfig,
    target: &str,
    command: Option<&str>,
    interactive: bool,
) -> Result<()> {
    let (cluster, task, container) = parse_target(target)?;
    let cmd = command.unwrap_or("/bin/sh");

    if interactive {
        // For interactive sessions, shell out to aws CLI (SSM plugin handles the WebSocket)
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
    } else {
        // Non-interactive: use SDK directly
        let ecs = EcsClient::new(config);
        let resp = ecs
            .execute_command()
            .cluster(cluster)
            .task(task)
            .container(container)
            .interactive(false)
            .command(cmd)
            .send()
            .await
            .context("ECS ExecuteCommand failed")?;

        if let Some(session) = resp.session() {
            eprintln!(
                "Session: {} (stream: {})",
                session.session_id().unwrap_or("?"),
                session.stream_url().unwrap_or("?")
            );
        }
    }

    Ok(())
}

use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn set(name: &str, target: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    cfg.aliases.insert(name.to_string(), target.to_string());
    cfg.save()?;
    eprintln!("✓ Alias '{name}' → {target}");
    Ok(())
}

pub async fn remove(name: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    if cfg.aliases.remove(name).is_some() {
        cfg.save()?;
        eprintln!("✓ Removed alias '{name}'");
    } else {
        eprintln!("Alias '{name}' not found");
    }
    Ok(())
}

pub async fn list() -> Result<()> {
    let cfg = Config::load()?;
    if cfg.aliases.is_empty() {
        eprintln!("No aliases configured.");
    } else {
        for (name, target) in &cfg.aliases {
            println!("{name:20} → {target}");
        }
    }
    Ok(())
}

/// Resolve an alias to "cluster/task_id/container" (ready for exec/cp/sync).
/// Alias format: cluster/service[/container[/task_id]]
/// - 2 parts (cluster/service): auto-resolve container + newest task
/// - 3 parts (cluster/service/container): auto-resolve newest task
/// - 4 parts (cluster/service/container/task_id): fully pinned
pub async fn resolve(config: &aws_config::SdkConfig, alias_or_target: &str) -> Result<String> {
    let cfg = Config::load()?;

    // Check if it's an alias
    let target = match cfg.aliases.get(alias_or_target) {
        Some(t) => t.clone(),
        None => return Ok(alias_or_target.to_string()), // Not an alias, pass through
    };

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    match parts.len() {
        4 => {
            // cluster/service/container/task_id — fully specified
            let (cluster, _service, container, task_id) = (parts[0], parts[1], parts[2], parts[3]);
            Ok(format!("{cluster}/{task_id}/{container}"))
        }
        3 => {
            // cluster/service/container — need to find newest running task
            let (cluster, service, container) = (parts[0], parts[1], parts[2]);
            let task_id = find_newest_task(config, cluster, service).await?;
            Ok(format!("{cluster}/{task_id}/{container}"))
        }
        2 => {
            // cluster/service — find newest task + resolve container name from task def
            let (cluster, service) = (parts[0], parts[1]);
            let (task_id, container) = find_newest_task_with_container(config, cluster, service).await?;
            Ok(format!("{cluster}/{task_id}/{container}"))
        }
        _ => anyhow::bail!("invalid alias target: '{target}' (expected cluster/service[/container[/task_id]])"),
    }
}

/// Find the newest RUNNING task ARN for a service, return just the task ID
async fn find_newest_task(config: &aws_config::SdkConfig, cluster: &str, service: &str) -> Result<String> {
    let (task_id, _) = find_newest_task_with_container(config, cluster, service).await?;
    Ok(task_id)
}

/// Find the newest RUNNING task for a service, return (task_id, container_name)
async fn find_newest_task_with_container(config: &aws_config::SdkConfig, cluster: &str, service: &str) -> Result<(String, String)> {
    let ecs = EcsClient::new(config);

    let resp = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(service)
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Running)
        .send()
        .await
        .context("ListTasks failed")?;

    let task_arns = resp.task_arns();
    if task_arns.is_empty() {
        anyhow::bail!("no RUNNING tasks found for service '{service}' in cluster '{cluster}'");
    }

    let desc = ecs
        .describe_tasks()
        .cluster(cluster)
        .set_tasks(Some(task_arns.to_vec()))
        .send()
        .await
        .context("DescribeTasks failed")?;

    let newest = desc
        .tasks()
        .iter()
        .filter(|t| t.last_status().map(|s| s.as_ref()) == Some("RUNNING"))
        .max_by_key(|t| t.started_at())
        .context("no RUNNING tasks found")?;

    // Extract task ID from ARN
    let arn = newest.task_arn().context("task has no ARN")?;
    let task_id = arn.rsplit('/').next().context("cannot parse task ID from ARN")?;

    // Get the app container name (skip ECS Service Connect sidecars)
    let container_name = newest
        .containers()
        .iter()
        .filter(|c| {
            let name = c.name().unwrap_or_default();
            !name.starts_with("ecs-service-connect-")
        })
        .next()
        .and_then(|c| c.name())
        .context("task has no app containers")?
        .to_string();

    Ok((task_id.to_string(), container_name))
}

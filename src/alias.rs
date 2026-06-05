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

/// Describe the resolved task for an alias
pub async fn describe(config: &aws_config::SdkConfig, alias_name: &str) -> Result<()> {
    let cfg = Config::load()?;
    let target = cfg
        .aliases
        .get(alias_name)
        .context(format!("alias '{alias_name}' not found"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2 => (parts[0], parts[1]),
        3 => (parts[0], parts[1]),
        4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target"),
    };

    let ecs = EcsClient::new(config);

    let tasks_resp = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(service)
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Running)
        .send()
        .await
        .context("ListTasks failed")?;

    let task_arns = tasks_resp.task_arns();
    if task_arns.is_empty() {
        println!("Alias:   {alias_name}");
        println!("Target:  {target}");
        println!("Status:  No RUNNING tasks");
        return Ok(());
    }

    let desc = ecs
        .describe_tasks()
        .cluster(cluster)
        .set_tasks(Some(task_arns.to_vec()))
        .send()
        .await
        .context("DescribeTasks failed")?;

    println!("Alias:   {alias_name}");
    println!("Target:  {target}");
    println!("Cluster: {cluster}");
    println!("Service: {service}");
    println!("Tasks:   {}", task_arns.len());
    println!();

    for task in desc.tasks() {
        let task_id = task
            .task_arn()
            .unwrap_or("?")
            .rsplit('/')
            .next()
            .unwrap_or("?");
        let status = task.last_status().unwrap_or("?");
        let health = task
            .health_status()
            .map(|h| h.as_str())
            .unwrap_or("UNKNOWN");
        let started = task
            .started_at()
            .map(|t| t.fmt(aws_sdk_ecs::primitives::DateTimeFormat::DateTime).unwrap_or_default())
            .unwrap_or_else(|| "-".to_string());
        let cpu = task.cpu().unwrap_or("?");
        let memory = task.memory().unwrap_or("?");
        let task_def_arn = task.task_definition_arn().unwrap_or("?");
        let capacity = task.capacity_provider_name().unwrap_or("?");
        let platform = task.platform_version().unwrap_or("?");
        let az = task.availability_zone().unwrap_or("?");
        let connectivity = task.connectivity().map(|c| c.as_str()).unwrap_or("?");
        let exec_enabled = task.enable_execute_command();

        // Fetch task definition for env vars + arch
        let task_def = ecs
            .describe_task_definition()
            .task_definition(task_def_arn)
            .send()
            .await
            .ok()
            .and_then(|r| r.task_definition);

        // Get runtime platform from task definition
        let arch = task_def.as_ref()
            .and_then(|td| td.runtime_platform().cloned())
            .and_then(|rp| rp.cpu_architecture().cloned())
            .map(|a| a.as_str().to_string())
            .unwrap_or_else(|| "X86_64".to_string());

        println!("  Task:       {task_id}");
        println!("  Status:     {status}");
        println!("  Health:     {health}");
        println!("  Started:    {started}");
        println!("  CPU/Memory: {cpu} / {memory}");
        println!("  Arch:       {arch}");
        println!("  Capacity:   {capacity}");
        println!("  Platform:   {platform}");
        println!("  AZ:         {az}");
        println!("  Connected:  {connectivity}");
        println!("  ExecEnabled:{exec_enabled}");
        println!("  TaskDef:    {task_def_arn}");
        println!("  Containers:");
        for c in task.containers() {
            let name = c.name().unwrap_or("?");
            let c_status = c.last_status().unwrap_or("?");
            let image = c.image().unwrap_or("?");
            let sidecar = if name.starts_with("ecs-service-connect-") {
                " (sidecar)"
            } else {
                ""
            };
            println!("    - {name}{sidecar}: {c_status} [{image}]");

            // Show env vars from task definition
            if !name.starts_with("ecs-service-connect-") {
                if let Some(ref td) = task_def {
                    if let Some(container_def) = td.container_definitions().iter().find(|cd| cd.name() == Some(name)) {
                        let env = container_def.environment();
                        let secrets = container_def.secrets();
                        if !env.is_empty() || !secrets.is_empty() {
                            println!("      Env:");
                            for kv in env {
                                let k = kv.name().unwrap_or("?");
                                let v = kv.value().unwrap_or("");
                                println!("        {k}={v}");
                            }
                            for s in secrets {
                                let k = s.name();
                                let from = s.value_from();
                                // Shorten ARN for display
                                let source = if from.contains(":secretsmanager:") {
                                    "secretsmanager"
                                } else if from.contains(":ssm:") {
                                    "ssm"
                                } else {
                                    "secret"
                                };
                                println!("        {k}=*** (from {source})");
                            }
                        }
                    }
                }
            }
        }
        println!();
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

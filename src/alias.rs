use anyhow::{Context, Result};
use aws_sdk_cloudwatchlogs::Client as LogsClient;
use aws_sdk_ec2::Client as Ec2Client;
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

/// Extract private IP and ENI ID from task attachment details.
fn extract_eni_details(task: &aws_sdk_ecs::types::Task) -> (Option<String>, Option<String>) {
    let mut private_ip = None;
    let mut eni_id = None;
    for attachment in task.attachments() {
        for detail in attachment.details() {
            match detail.name().unwrap_or_default() {
                "privateIPv4Address" => private_ip = detail.value().map(|v| v.to_string()),
                "networkInterfaceId" => eni_id = detail.value().map(|v| v.to_string()),
                _ => {}
            }
        }
    }
    (private_ip, eni_id)
}

/// Look up public IP for an ENI.
async fn lookup_public_ip(ec2: &Ec2Client, eni_id: &str) -> Option<String> {
    ec2.describe_network_interfaces()
        .network_interface_ids(eni_id)
        .send()
        .await
        .ok()?
        .network_interfaces()
        .first()?
        .association()
        .and_then(|a| a.public_ip())
        .map(|s| s.to_string())
}

/// Describe the resolved task for an alias
pub async fn describe(
    config: &aws_config::SdkConfig,
    alias_name: &str,
    output: Option<&str>,
) -> Result<()> {
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

    if let Some(fmt) = output {
        if fmt == "json" || fmt == "--json" {
            return print_json(config, &ecs, alias_name, &target, cluster, service, &desc).await;
        } else if let Some(template) = fmt.strip_prefix("jsonpath=") {
            let template = template.trim_matches('\'');
            return print_jsonpath(
                config, &ecs, alias_name, &target, cluster, service, &desc, template,
            )
            .await;
        } else {
            anyhow::bail!(
                "unknown output format '{}': use 'json' or \"jsonpath='<template>'\"",
                fmt
            );
        }
    }

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
            .map(|t| {
                t.fmt(aws_sdk_ecs::primitives::DateTimeFormat::DateTime)
                    .unwrap_or_default()
            })
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
        let arch = task_def
            .as_ref()
            .and_then(|td| td.runtime_platform().cloned())
            .and_then(|rp| rp.cpu_architecture().cloned())
            .map(|a| a.as_str().to_string())
            .unwrap_or_else(|| "X86_64".to_string());

        println!("  Task:       {task_id}");
        println!("  Status:     {status}");

        // Show IPs
        let (private_ip, eni_id) = extract_eni_details(task);
        if let Some(ref ip) = private_ip {
            println!("  PrivateIP:  {ip}");
        }
        if let Some(ref eni) = eni_id {
            let ec2 = Ec2Client::new(config);
            if let Some(pub_ip) = lookup_public_ip(&ec2, eni).await {
                println!("  PublicIP:   {pub_ip}");
            }
        }

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
                    if let Some(container_def) = td
                        .container_definitions()
                        .iter()
                        .find(|cd| cd.name() == Some(name))
                    {
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

        // Tail last 10 log lines
        if let Some(ref td) = task_def {
            let app_container = td.container_definitions().iter().find(|cd| {
                !cd.name()
                    .unwrap_or_default()
                    .starts_with("ecs-service-connect-")
            });
            if let Some(cd) = app_container {
                if let Some(log_config) = cd.log_configuration() {
                    if log_config.log_driver().as_str() == "awslogs" {
                        if let Some(opts) = log_config.options() {
                            if let (Some(group), Some(prefix)) =
                                (opts.get("awslogs-group"), opts.get("awslogs-stream-prefix"))
                            {
                                let container_name = cd.name().unwrap_or("app");
                                let stream_name = format!("{prefix}/{container_name}/{task_id}");
                                let logs = LogsClient::new(config);
                                if let Ok(resp) = logs
                                    .get_log_events()
                                    .log_group_name(group)
                                    .log_stream_name(&stream_name)
                                    .limit(10)
                                    .start_from_head(false)
                                    .send()
                                    .await
                                {
                                    let events = resp.events();
                                    if !events.is_empty() {
                                        println!("  Logs (last {}):", events.len());
                                        for event in events {
                                            let msg = event.message().unwrap_or("");
                                            println!("    {msg}");
                                        }
                                        println!();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn print_json(
    config: &aws_config::SdkConfig,
    ecs: &EcsClient,
    alias_name: &str,
    target: &str,
    cluster: &str,
    service: &str,
    desc: &aws_sdk_ecs::operation::describe_tasks::DescribeTasksOutput,
) -> Result<()> {
    let output = build_json(config, ecs, alias_name, target, cluster, service, desc).await?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn build_json(
    config: &aws_config::SdkConfig,
    ecs: &EcsClient,
    alias_name: &str,
    target: &str,
    cluster: &str,
    service: &str,
    desc: &aws_sdk_ecs::operation::describe_tasks::DescribeTasksOutput,
) -> Result<serde_json::Value> {
    let mut tasks_json = Vec::new();

    for task in desc.tasks() {
        let task_id = task
            .task_arn()
            .unwrap_or("?")
            .rsplit('/')
            .next()
            .unwrap_or("?");
        let task_def_arn = task.task_definition_arn().unwrap_or("?");

        let task_def = ecs
            .describe_task_definition()
            .task_definition(task_def_arn)
            .send()
            .await
            .ok()
            .and_then(|r| r.task_definition);

        let arch = task_def
            .as_ref()
            .and_then(|td| td.runtime_platform().cloned())
            .and_then(|rp| rp.cpu_architecture().cloned())
            .map(|a| a.as_str().to_string())
            .unwrap_or_else(|| "X86_64".to_string());

        let mut containers_json = Vec::new();
        for c in task.containers() {
            let name = c.name().unwrap_or("?");
            let is_sidecar = name.starts_with("ecs-service-connect-");
            let mut cj = serde_json::json!({
                "name": name,
                "status": c.last_status().unwrap_or("?"),
                "image": c.image().unwrap_or("?"),
                "sidecar": is_sidecar,
            });

            if !is_sidecar {
                if let Some(ref td) = task_def {
                    if let Some(cd) = td
                        .container_definitions()
                        .iter()
                        .find(|cd| cd.name() == Some(name))
                    {
                        let mut env = serde_json::Map::new();
                        for kv in cd.environment() {
                            let k = kv.name().unwrap_or("?");
                            let v = kv.value().unwrap_or("");
                            env.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                        }
                        let mut secrets = serde_json::Map::new();
                        for s in cd.secrets() {
                            secrets.insert(
                                s.name().to_string(),
                                serde_json::Value::String(s.value_from().to_string()),
                            );
                        }
                        cj["env"] = serde_json::Value::Object(env);
                        cj["secrets"] = serde_json::Value::Object(secrets);
                    }
                }
            }
            containers_json.push(cj);
        }

        let (private_ip, eni_id) = extract_eni_details(task);
        let public_ip = if let Some(ref eni) = eni_id {
            let ec2 = Ec2Client::new(config);
            lookup_public_ip(&ec2, eni).await
        } else {
            None
        };

        tasks_json.push(serde_json::json!({
            "task_id": task_id,
            "status": task.last_status().unwrap_or("?"),
            "health": task.health_status().map(|h| h.as_str()).unwrap_or("UNKNOWN"),
            "started": task.started_at().map(|t| t.fmt(aws_sdk_ecs::primitives::DateTimeFormat::DateTime).unwrap_or_default()).unwrap_or_default(),
            "cpu": task.cpu().unwrap_or("?"),
            "memory": task.memory().unwrap_or("?"),
            "arch": arch,
            "capacity": task.capacity_provider_name().unwrap_or("?"),
            "platform_version": task.platform_version().unwrap_or("?"),
            "az": task.availability_zone().unwrap_or("?"),
            "connectivity": task.connectivity().map(|c| c.as_str()).unwrap_or("?"),
            "exec_enabled": task.enable_execute_command(),
            "task_definition": task_def_arn,
            "private_ip": private_ip,
            "public_ip": public_ip,
            "containers": containers_json,
        }));
    }

    Ok(serde_json::json!({
        "alias": alias_name,
        "target": target,
        "cluster": cluster,
        "service": service,
        "tasks": tasks_json,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn print_jsonpath(
    config: &aws_config::SdkConfig,
    ecs: &EcsClient,
    alias_name: &str,
    target: &str,
    cluster: &str,
    service: &str,
    desc: &aws_sdk_ecs::operation::describe_tasks::DescribeTasksOutput,
    template: &str,
) -> Result<()> {
    // Build the same JSON as print_json, then evaluate the template
    let json_value = build_json(config, ecs, alias_name, target, cluster, service, desc).await?;

    // Replace {.path.to.field} or {.path[index].field} with resolved values
    let result = resolve_jsonpath_template(template, &json_value);
    println!("{result}");
    Ok(())
}

/// Resolve a jsonpath template: replaces `{.path}` expressions with values from JSON.
fn resolve_jsonpath_template(template: &str, value: &serde_json::Value) -> String {
    let mut result = String::new();
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let after_brace = &rest[start + 1..];
        if let Some(end) = after_brace.find('}') {
            let path = &after_brace[..end];
            let resolved = resolve_path(path.trim(), value);
            result.push_str(&resolved);
            rest = &after_brace[end + 1..];
        } else {
            result.push('{');
            rest = after_brace;
        }
    }
    result.push_str(rest);
    result
}

/// Resolve a dot-path like `.tasks[0].public_ip` against a JSON value.
fn resolve_path(path: &str, value: &serde_json::Value) -> String {
    let path = path.strip_prefix('.').unwrap_or(path);
    let mut current = value;

    for segment in split_path_segments(path) {
        match segment {
            PathSegment::Key(key) => {
                current = &current[key];
            }
            PathSegment::Index(key, idx) => {
                current = &current[key][idx];
            }
        }
        if current.is_null() {
            return String::new();
        }
    }

    match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

enum PathSegment<'a> {
    Key(&'a str),
    Index(&'a str, usize),
}

fn split_path_segments(path: &str) -> Vec<PathSegment<'_>> {
    let mut segments = Vec::new();
    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        if let Some(bracket) = part.find('[') {
            let key = &part[..bracket];
            let idx_str = &part[bracket + 1..part.len() - 1];
            if let Ok(idx) = idx_str.parse::<usize>() {
                segments.push(PathSegment::Index(key, idx));
            } else {
                segments.push(PathSegment::Key(part));
            }
        } else {
            segments.push(PathSegment::Key(part));
        }
    }
    segments
}

/// List all aliased services in a table with status info.
pub async fn list_all(config: &aws_config::SdkConfig, watch: bool) -> Result<()> {
    let cfg = Config::load()?;
    if cfg.aliases.is_empty() {
        eprintln!("No aliases configured.");
        return Ok(());
    }

    let ecs = EcsClient::new(config);
    let mut aliases: Vec<_> = cfg.aliases.iter().collect();
    aliases.sort_by_key(|(name, _)| name.to_lowercase());

    if !watch {
        let rows = fetch_all_rows(&ecs, &aliases).await;
        print_table(&rows);
        return Ok(());
    }

    // Watch mode: clear screen, print, sleep, repeat
    let mut prev_rows: Vec<ServiceRow> = Vec::new();
    loop {
        let rows = fetch_all_rows(&ecs, &aliases).await;
        if rows != prev_rows {
            // Move cursor to top and clear screen
            print!("\x1b[H\x1b[J");
            print_table(&rows);
            eprintln!("\nRefreshing every 5s... (Ctrl+C to stop)");
            prev_rows = rows;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ServiceRow {
    name: String,
    status: String,
    cpu: String,
    memory: String,
    capacity: String,
    image: String,
    tasks: usize,
}

fn colorize_status(status: &str) -> String {
    let color = if status == "RUNNING" {
        "\x1b[32m" // green
    } else if status.starts_with("PENDING") || status == "ACTIVATING" || status == "PROVISIONING" {
        "\x1b[33m" // yellow
    } else if status == "STOPPED"
        || status.starts_with("PARTIAL")
        || status.starts_with("STOPPING")
        || status == "ERROR"
        || status == "INVALID"
        || status == "NOT FOUND"
    {
        "\x1b[31m" // red
    } else {
        "\x1b[0m"
    };
    format!("{}{:<12}\x1b[0m", color, status)
}

fn print_table(rows: &[ServiceRow]) {
    println!(
        "{:<15} {:<12} {:<6} {:<6} {:<15} {:<30} {:<5}",
        "NAME", "STATUS", "CPU", "MEM", "CAPACITY", "IMAGE", "TASKS"
    );
    for r in rows {
        print!("{:<15} ", r.name);
        print!("{} ", colorize_status(&r.status));
        println!(
            "{:<6} {:<6} {:<15} {:<30} {}",
            r.cpu, r.memory, r.capacity, r.image, r.tasks
        );
    }
}

async fn fetch_all_rows(ecs: &EcsClient, aliases: &[(&String, &String)]) -> Vec<ServiceRow> {
    let mut rows = Vec::new();
    for (name, target) in aliases {
        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = match parts.len() {
            2..=4 => (parts[0], parts[1]),
            _ => {
                rows.push(ServiceRow {
                    name: name.to_string(),
                    status: "INVALID".into(),
                    cpu: "-".into(),
                    memory: "-".into(),
                    capacity: "-".into(),
                    image: "-".into(),
                    tasks: 0,
                });
                continue;
            }
        };

        // Use describe_services for accurate status
        let svc_resp = ecs
            .describe_services()
            .cluster(cluster)
            .services(service)
            .send()
            .await;

        let svc = match svc_resp {
            Ok(r) => match r.services().first() {
                Some(s) => s.clone(),
                None => {
                    rows.push(ServiceRow {
                        name: name.to_string(),
                        status: "NOT FOUND".into(),
                        cpu: "-".into(),
                        memory: "-".into(),
                        capacity: "-".into(),
                        image: "-".into(),
                        tasks: 0,
                    });
                    continue;
                }
            },
            Err(_) => {
                rows.push(ServiceRow {
                    name: name.to_string(),
                    status: "ERROR".into(),
                    cpu: "-".into(),
                    memory: "-".into(),
                    capacity: "-".into(),
                    image: "-".into(),
                    tasks: 0,
                });
                continue;
            }
        };

        let running = svc.running_count() as usize;
        let desired = svc.desired_count() as usize;
        let pending = svc.pending_count() as usize;

        // Determine status from service state
        let status = if desired == 0 {
            "STOPPED".to_string()
        } else if running == desired && pending == 0 {
            "RUNNING".to_string()
        } else if pending > 0 {
            format!("PENDING({})", pending)
        } else if running < desired {
            format!("PARTIAL({}/{})", running, desired)
        } else {
            svc.status().unwrap_or("UNKNOWN").to_string()
        };

        // Get task details for cpu/mem/image from task definition
        let task_def_arn = svc
            .deployments()
            .first()
            .and_then(|d| d.task_definition())
            .unwrap_or("-");

        let (cpu, memory, image, capacity) = if task_def_arn != "-" {
            if let Ok(td_resp) = ecs
                .describe_task_definition()
                .task_definition(task_def_arn)
                .send()
                .await
            {
                let td = td_resp.task_definition();
                let cpu = td.and_then(|t| t.cpu()).unwrap_or("-").to_string();
                let mem = td.and_then(|t| t.memory()).unwrap_or("-").to_string();
                let img = td
                    .map(|t| {
                        t.container_definitions()
                            .iter()
                            .find(|c| {
                                !c.name()
                                    .unwrap_or_default()
                                    .starts_with("ecs-service-connect-")
                            })
                            .and_then(|c| c.image())
                            .unwrap_or("-")
                            .to_string()
                    })
                    .unwrap_or_else(|| "-".to_string());
                let cap = svc
                    .deployments()
                    .first()
                    .and_then(|d| d.capacity_provider_strategy().first())
                    .map(|s| s.capacity_provider().to_string())
                    .unwrap_or_else(|| "-".to_string());
                (cpu, mem, img, cap)
            } else {
                ("-".into(), "-".into(), "-".into(), "-".into())
            }
        } else {
            ("-".into(), "-".into(), "-".into(), "-".into())
        };

        let image_short = image.rsplit('/').next().unwrap_or(&image);
        let image_display = if image_short.len() > 30 {
            &image_short[..30]
        } else {
            image_short
        };

        rows.push(ServiceRow {
            name: name.to_string(),
            status,
            cpu,
            memory,
            capacity,
            image: image_display.to_string(),
            tasks: running,
        });
    }
    rows
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
            let (task_id, container) =
                find_newest_task_with_container(config, cluster, service).await?;
            Ok(format!("{cluster}/{task_id}/{container}"))
        }
        _ => anyhow::bail!(
            "invalid alias target: '{target}' (expected cluster/service[/container[/task_id]])"
        ),
    }
}

/// Find the newest RUNNING task ARN for a service, return just the task ID
async fn find_newest_task(
    config: &aws_config::SdkConfig,
    cluster: &str,
    service: &str,
) -> Result<String> {
    let (task_id, _) = find_newest_task_with_container(config, cluster, service).await?;
    Ok(task_id)
}

/// Find the newest RUNNING task for a service, return (task_id, container_name)
async fn find_newest_task_with_container(
    config: &aws_config::SdkConfig,
    cluster: &str,
    service: &str,
) -> Result<(String, String)> {
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
        .filter(|t| t.last_status() == Some("RUNNING"))
        .max_by_key(|t| t.started_at())
        .context("no RUNNING tasks found")?;

    // Extract task ID from ARN
    let arn = newest.task_arn().context("task has no ARN")?;
    let task_id = arn
        .rsplit('/')
        .next()
        .context("cannot parse task ID from ARN")?;

    // Get the app container name (skip ECS Service Connect sidecars)
    let container_name = newest
        .containers()
        .iter()
        .find(|c| {
            !c.name()
                .unwrap_or_default()
                .starts_with("ecs-service-connect-")
        })
        .and_then(|c| c.name())
        .context("task has no app containers")?
        .to_string();

    Ok((task_id.to_string(), container_name))
}

/// Resolve alias in a remote path string like "alias:/path" → "cluster/task/container:/path".
/// If the string doesn't contain ':', or the prefix is already a full path (contains '/'),
/// returns the original string unchanged.
pub async fn resolve_remote(config: &aws_config::SdkConfig, s: &str) -> anyhow::Result<String> {
    if let Some(colon_pos) = s.find(':') {
        let prefix = &s[..colon_pos];
        let path = &s[colon_pos..]; // includes the ':'
        if !prefix.contains('/') {
            let resolved = resolve(config, prefix).await?;
            if resolved != prefix {
                return Ok(format!("{resolved}{path}"));
            }
        }
    }
    Ok(s.to_string())
}

use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(config: &aws_config::SdkConfig, name: &str, count: i32, wait: bool) -> Result<()> {
    if count < 0 {
        anyhow::bail!("count must be >= 0, got {count}");
    }

    let cfg = Config::load()?;
    let targets = cfg.resolve_targets(name);

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", name);
    }

    let ecs = EcsClient::new(config);

    for alias in &targets {
        scale_alias(&ecs, &cfg, alias, count).await?;
    }

    if wait && targets.len() == 1 {
        let alias = &targets[0];
        let target = cfg.aliases.get(alias).unwrap();
        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = (parts[0], parts[1]);
        eprintln!("⏳ Waiting for service to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Service stable");
    }

    Ok(())
}

/// Scale a single alias by resolving it to cluster/service.
pub async fn scale_service(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    count: i32,
    force: bool,
) -> Result<()> {
    let mut req = ecs
        .update_service()
        .cluster(cluster)
        .service(service)
        .desired_count(count);
    if force {
        req = req.force_new_deployment(true);
    }
    req.send()
        .await
        .context(format!("UpdateService failed for {}/{}", cluster, service))?;
    Ok(())
}

async fn scale_alias(ecs: &EcsClient, cfg: &Config, alias: &str, count: i32) -> Result<()> {
    let target = cfg
        .aliases
        .get(alias)
        .context(format!("alias '{alias}' not found"))?;

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2..=4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target for '{alias}'"),
    };

    ecs.update_service()
        .cluster(cluster)
        .service(service)
        .desired_count(count)
        .force_new_deployment(true)
        .send()
        .await
        .context(format!("UpdateService failed for {alias}"))?;

    eprintln!("✓ {alias} → desired_count={count}");
    Ok(())
}

/// Create EventBridge Scheduler schedules for a service or @group.
pub async fn run_with_schedule(
    aws_config: &aws_config::SdkConfig,
    name: &str,
    count: i32,
    schedule_expression: &str,
    timezone: &str,
) -> Result<()> {
    if count < 0 {
        anyhow::bail!("count must be >= 0, got {count}");
    }
    validate_schedule_expression(schedule_expression)?;

    let cfg = Config::load()?;
    let targets = cfg.resolve_targets(name);

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", name);
    }

    let scheduler = aws_sdk_scheduler::Client::new(aws_config);
    let sts = aws_sdk_sts::Client::new(aws_config);
    let iam = aws_sdk_iam::Client::new(aws_config);

    let identity = sts
        .get_caller_identity()
        .send()
        .await
        .context("failed to get caller identity")?;
    let account_id = identity.account().unwrap_or("unknown");
    let region = aws_config
        .region()
        .map(|r| r.to_string())
        .unwrap_or_else(|| "us-east-1".to_string());

    let group_name = "ecsctl-schedules";
    ensure_schedule_group(&scheduler, group_name).await?;
    let role_arn = ensure_scheduler_role(&iam, account_id, &region).await?;

    for alias in &targets {
        let target = cfg
            .aliases
            .get(alias)
            .context(format!("alias '{alias}' not found"))?;
        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = match parts.len() {
            2..=4 => (parts[0], parts[1]),
            _ => anyhow::bail!("invalid alias target for '{alias}'"),
        };

        let safe_alias = alias.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '.',
            "-",
        );
        let suffix = format!("-to-{}", count);
        let prefix = "ecsctl-scale-";
        let max_alias_len = 64 - prefix.len() - suffix.len();
        let truncated = if safe_alias.len() > max_alias_len {
            &safe_alias[..max_alias_len]
        } else {
            &safe_alias
        };
        let schedule_name = format!("{}{}{}", prefix, truncated, suffix);

        let target_input = serde_json::json!({
            "Cluster": cluster,
            "Service": service,
            "DesiredCount": count
        });

        let target = aws_sdk_scheduler::types::Target::builder()
            .arn("arn:aws:scheduler:::aws-sdk:ecs:updateService")
            .role_arn(&role_arn)
            .input(target_input.to_string())
            .build()?;

        let ftw = aws_sdk_scheduler::types::FlexibleTimeWindow::builder()
            .mode(aws_sdk_scheduler::types::FlexibleTimeWindowMode::Off)
            .build()?;

        let exists = schedule_exists(&scheduler, &schedule_name, group_name).await?;

        if exists {
            scheduler
                .update_schedule()
                .name(&schedule_name)
                .group_name(group_name)
                .schedule_expression(schedule_expression)
                .schedule_expression_timezone(timezone)
                .flexible_time_window(ftw)
                .target(target)
                .send()
                .await
                .context("failed to update schedule")?;
            eprintln!("✓ Updated: {schedule_name}");
        } else {
            scheduler
                .create_schedule()
                .name(&schedule_name)
                .group_name(group_name)
                .schedule_expression(schedule_expression)
                .schedule_expression_timezone(timezone)
                .flexible_time_window(ftw)
                .target(target)
                .send()
                .await
                .context("failed to create schedule")?;
            eprintln!("✓ Created: {schedule_name}");
        }

        eprintln!("  Expression: {schedule_expression} ({timezone})");
        eprintln!("  Action:     scale {alias} ({service}) to {count}");
    }

    eprintln!("\n  Use 'ecsctl schedule list' to view all schedules");
    Ok(())
}

/// List all schedules in the ecsctl-schedules group.
pub async fn list_schedules(aws_config: &aws_config::SdkConfig) -> Result<()> {
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);
    let group_name = "ecsctl-schedules";

    let mut all = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let mut req = scheduler.list_schedules().group_name(group_name);
        if let Some(token) = &next_token {
            req = req.next_token(token);
        }
        match req.send().await {
            Ok(output) => {
                all.extend(output.schedules().to_vec());
                next_token = output.next_token().map(|s| s.to_string());
                if next_token.is_none() {
                    break;
                }
            }
            Err(e) => {
                if e.as_service_error()
                    .map(|se| se.is_resource_not_found_exception())
                    .unwrap_or(false)
                {
                    println!("No schedules found (group '{group_name}' does not exist yet).");
                    return Ok(());
                }
                return Err(e).context("failed to list schedules");
            }
        }
    }

    if all.is_empty() {
        println!("No schedules found.");
        return Ok(());
    }

    println!("{:<45} {:<30} {:<18} STATE", "NAME", "SCHEDULE", "TIMEZONE");
    for s in &all {
        let name = s.name().unwrap_or("-");
        let state = s.state().map(|st| st.as_str()).unwrap_or("?");
        let (expr, tz) = match scheduler
            .get_schedule()
            .name(name)
            .group_name(group_name)
            .send()
            .await
        {
            Ok(detail) => (
                detail.schedule_expression().unwrap_or("-").to_string(),
                detail
                    .schedule_expression_timezone()
                    .unwrap_or("UTC")
                    .to_string(),
            ),
            Err(_) => ("-".to_string(), "-".to_string()),
        };
        println!("{:<45} {:<30} {:<18} {}", name, expr, tz, state);
    }
    Ok(())
}

/// Delete a schedule by name.
pub async fn delete_schedule(aws_config: &aws_config::SdkConfig, name: &str) -> Result<()> {
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);
    let group_name = "ecsctl-schedules";

    match scheduler
        .delete_schedule()
        .name(name)
        .group_name(group_name)
        .send()
        .await
    {
        Ok(_) => eprintln!("✓ Deleted: {name}"),
        Err(e) => {
            if e.as_service_error()
                .map(|se| se.is_resource_not_found_exception())
                .unwrap_or(false)
            {
                anyhow::bail!("schedule '{name}' not found in group '{group_name}'");
            }
            return Err(e).context("failed to delete schedule");
        }
    }
    Ok(())
}

// --- Helpers ---

fn validate_schedule_expression(expr: &str) -> Result<()> {
    let trimmed = expr.trim();
    if trimmed.starts_with("cron(") && trimmed.ends_with(')') {
        let inner = &trimmed[5..trimmed.len() - 1];
        let fields: Vec<&str> = inner.split_whitespace().collect();
        if fields.len() != 6 {
            anyhow::bail!(
                "invalid cron: expected 6 fields (min hour dom month dow year), got {}. Example: cron(0 8 * * ? *)",
                fields.len()
            );
        }
    } else if trimmed.starts_with("rate(") && trimmed.ends_with(')') {
        let inner = &trimmed[5..trimmed.len() - 1].trim();
        if inner.is_empty() {
            anyhow::bail!("invalid rate expression: rate() is empty");
        }
    } else if trimmed.starts_with("at(") && trimmed.ends_with(')') {
        // at() one-time — basic validation
    } else {
        anyhow::bail!(
            "invalid schedule expression. Must start with cron(...), rate(...), or at(...). Got: '{trimmed}'"
        );
    }
    Ok(())
}

async fn schedule_exists(
    scheduler: &aws_sdk_scheduler::Client,
    name: &str,
    group_name: &str,
) -> Result<bool> {
    match scheduler
        .get_schedule()
        .name(name)
        .group_name(group_name)
        .send()
        .await
    {
        Ok(_) => Ok(true),
        Err(e) => {
            if e.as_service_error()
                .map(|se| se.is_resource_not_found_exception())
                .unwrap_or(false)
            {
                Ok(false)
            } else {
                Err(e).context("failed to check schedule existence")
            }
        }
    }
}

async fn ensure_schedule_group(
    scheduler: &aws_sdk_scheduler::Client,
    group_name: &str,
) -> Result<()> {
    if scheduler
        .get_schedule_group()
        .name(group_name)
        .send()
        .await
        .is_err()
    {
        let result = scheduler
            .create_schedule_group()
            .name(group_name)
            .send()
            .await;
        if let Err(e) = result {
            if !e
                .as_service_error()
                .map(|se| se.is_conflict_exception())
                .unwrap_or(false)
            {
                anyhow::bail!("failed to create schedule group: {e}");
            }
        }
    }
    Ok(())
}

async fn ensure_scheduler_role(
    iam: &aws_sdk_iam::Client,
    account_id: &str,
    region: &str,
) -> Result<String> {
    let role_name = "ecsctl-scheduler-role";
    let role_arn = format!("arn:aws:iam::{account_id}:role/{role_name}");

    if iam.get_role().role_name(role_name).send().await.is_ok() {
        return Ok(role_arn);
    }

    let trust_policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": {"Service": "scheduler.amazonaws.com"},
            "Action": "sts:AssumeRole",
            "Condition": {
                "StringEquals": {
                    "aws:SourceAccount": account_id,
                    "aws:SourceArn": format!("arn:aws:scheduler:{region}:{account_id}:schedule-group/ecsctl-schedules")
                }
            }
        }]
    });

    iam.create_role()
        .role_name(role_name)
        .assume_role_policy_document(trust_policy.to_string())
        .description("EventBridge Scheduler role for ecsctl scale commands")
        .send()
        .await
        .context("failed to create scheduler role")?;

    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": "ecs:UpdateService",
            "Resource": format!("arn:aws:ecs:{region}:{account_id}:service/*/*")
        }]
    });

    iam.put_role_policy()
        .role_name(role_name)
        .policy_name("ecs-scale")
        .policy_document(policy.to_string())
        .send()
        .await
        .context("failed to attach policy to scheduler role")?;

    // Wait for IAM propagation
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    Ok(role_arn)
}

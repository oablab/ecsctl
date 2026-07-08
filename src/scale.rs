use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use aws_sdk_scheduler::error::ProvideErrorMetadata;

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
        let (cluster, service) = resolve_alias(&cfg, alias)?;
        ecs.update_service()
            .cluster(cluster)
            .service(service)
            .desired_count(count)
            .force_new_deployment(true)
            .send()
            .await
            .context(format!("UpdateService failed for {alias}"))?;
        eprintln!("✓ {alias} → desired_count={count}");
    }

    if wait && targets.len() == 1 {
        let alias = &targets[0];
        let (cluster, service) = resolve_alias(&cfg, alias)?;
        eprintln!("⏳ Waiting for service to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Service stable");
    }

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

    // Collect cluster ARNs for scoped IAM policy
    let mut cluster_arns: Vec<String> = Vec::new();
    for alias in &targets {
        let (cluster, _service) = resolve_alias(&cfg, alias)?;
        let arn = format!("arn:aws:ecs:{region}:{account_id}:service/{cluster}/*");
        if !cluster_arns.contains(&arn) {
            cluster_arns.push(arn);
        }
    }

    let group_name = "ecsctl-schedules";
    ensure_schedule_group(&scheduler, group_name).await?;
    let role_arn = ensure_scheduler_role(&iam, account_id, &region, &cluster_arns).await?;

    for alias in &targets {
        let (cluster, service) = resolve_alias(&cfg, alias)?;

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
            create_schedule_with_retry(
                &scheduler,
                &schedule_name,
                group_name,
                schedule_expression,
                timezone,
                ftw,
                target,
            )
            .await?;
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

/// Resolve an alias to (cluster, service) from config.
fn resolve_alias<'a>(cfg: &'a Config, alias: &str) -> Result<(&'a str, &'a str)> {
    let target = cfg
        .aliases
        .get(alias)
        .context(format!("alias '{alias}' not found"))?;

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    match parts.len() {
        2..=4 => Ok((parts[0], parts[1])),
        _ => anyhow::bail!(
            "invalid alias target for '{alias}': expected 'cluster/service', got '{target}'"
        ),
    }
}

fn validate_schedule_expression(expr: &str) -> Result<()> {
    let trimmed = expr.trim();
    if trimmed.starts_with("cron(") && trimmed.ends_with(')') {
        let inner = &trimmed[5..trimmed.len() - 1];
        let fields: Vec<&str> = inner.split_whitespace().collect();
        if fields.len() != 6 {
            anyhow::bail!(
                "invalid cron: expected 6 fields (min hour dom month dow year), got {}. \
                 Example: cron(0 8 * * ? *)",
                fields.len()
            );
        }
    } else if trimmed.starts_with("rate(") && trimmed.ends_with(')') {
        let inner = trimmed[5..trimmed.len() - 1].trim();
        if inner.is_empty() {
            anyhow::bail!("invalid rate expression: rate() is empty");
        }
        // Validate rate format: <number> <unit>
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "invalid rate expression: expected 'rate(<value> <unit>)', got 'rate({inner})'. \
                 Example: rate(5 minutes)"
            );
        }
        if parts[0].parse::<u64>().is_err() {
            anyhow::bail!(
                "invalid rate expression: value '{}' is not a positive integer",
                parts[0]
            );
        }
        let valid_units = ["minute", "minutes", "hour", "hours", "day", "days"];
        if !valid_units.contains(&parts[1]) {
            anyhow::bail!(
                "invalid rate expression: unit '{}' not recognized. \
                 Valid units: minute(s), hour(s), day(s)",
                parts[1]
            );
        }
    } else if trimmed.starts_with("at(") && trimmed.ends_with(')') {
        let inner = trimmed[3..trimmed.len() - 1].trim();
        if inner.is_empty() {
            anyhow::bail!(
                "invalid at expression: at() is empty. \
                 Example: at(2024-01-01T00:00:00)"
            );
        }
    } else {
        anyhow::bail!(
            "invalid schedule expression. Must start with cron(...), rate(...), or at(...). \
             Got: '{trimmed}'"
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

/// Create a schedule with retry+backoff to handle IAM propagation delay.
async fn create_schedule_with_retry(
    scheduler: &aws_sdk_scheduler::Client,
    schedule_name: &str,
    group_name: &str,
    schedule_expression: &str,
    timezone: &str,
    ftw: aws_sdk_scheduler::types::FlexibleTimeWindow,
    target: aws_sdk_scheduler::types::Target,
) -> Result<()> {
    let max_attempts = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let ftw_clone = ftw.clone();
        let target_clone = target.clone();
        match scheduler
            .create_schedule()
            .name(schedule_name)
            .group_name(group_name)
            .schedule_expression(schedule_expression)
            .schedule_expression_timezone(timezone)
            .flexible_time_window(ftw_clone)
            .target(target_clone)
            .send()
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                // Retry on role-not-ready errors (IAM propagation)
                let is_retryable = e
                    .as_service_error()
                    .map(|se| {
                        let msg = se.message().unwrap_or("");
                        msg.contains("role") || msg.contains("unable to assume")
                    })
                    .unwrap_or(false);

                if is_retryable && attempt < max_attempts {
                    let delay = std::time::Duration::from_secs(5 * attempt as u64);
                    eprintln!(
                        "  ⏳ IAM role not yet propagated, retrying in {}s (attempt {}/{})",
                        delay.as_secs(),
                        attempt,
                        max_attempts
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    return Err(e).context("failed to create schedule");
                }
            }
        }
    }
}

async fn ensure_schedule_group(
    scheduler: &aws_sdk_scheduler::Client,
    group_name: &str,
) -> Result<()> {
    // Check if group exists, discriminating ResourceNotFoundException from other errors
    match scheduler.get_schedule_group().name(group_name).send().await {
        Ok(_) => return Ok(()),
        Err(e) => {
            if !e
                .as_service_error()
                .map(|se| se.is_resource_not_found_exception())
                .unwrap_or(false)
            {
                return Err(e).context("failed to check schedule group");
            }
            // ResourceNotFoundException — proceed to create
        }
    }

    // Create group, handle race condition (ConflictException = already exists)
    match scheduler
        .create_schedule_group()
        .name(group_name)
        .send()
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.as_service_error()
                .map(|se| se.is_conflict_exception())
                .unwrap_or(false)
            {
                Ok(()) // Another process created it — safe
            } else {
                Err(e).context("failed to create schedule group")
            }
        }
    }
}

async fn ensure_scheduler_role(
    iam: &aws_sdk_iam::Client,
    account_id: &str,
    region: &str,
    cluster_arns: &[String],
) -> Result<String> {
    let role_name = "ecsctl-scheduler-role";
    let role_arn = format!("arn:aws:iam::{account_id}:role/{role_name}");

    // C1: Discriminate NoSuchEntity from other errors
    match iam.get_role().role_name(role_name).send().await {
        Ok(_) => return Ok(role_arn),
        Err(e) => {
            if !e
                .as_service_error()
                .map(|se| se.is_no_such_entity_exception())
                .unwrap_or(false)
            {
                return Err(e).context("failed to check scheduler role");
            }
            // NoSuchEntity — proceed to create
        }
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

    // C2: Handle EntityAlreadyExistsException (race condition)
    match iam
        .create_role()
        .role_name(role_name)
        .assume_role_policy_document(trust_policy.to_string())
        .description("EventBridge Scheduler role for ecsctl scale commands")
        .send()
        .await
    {
        Ok(_) => {}
        Err(e) => {
            if e.as_service_error()
                .map(|se| se.is_entity_already_exists_exception())
                .unwrap_or(false)
            {
                // Another process created it — safe to continue
                return Ok(role_arn);
            }
            return Err(e).context("failed to create scheduler role");
        }
    }

    // C4: Scope IAM policy to specific clusters instead of wildcard
    let resources: Vec<String> = if cluster_arns.is_empty() {
        vec![format!("arn:aws:ecs:{region}:{account_id}:service/*/*")]
    } else {
        cluster_arns.to_vec()
    };

    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": "ecs:UpdateService",
            "Resource": resources
        }]
    });

    iam.put_role_policy()
        .role_name(role_name)
        .policy_name("ecs-scale")
        .policy_document(policy.to_string())
        .send()
        .await
        .context("failed to attach policy to scheduler role")?;

    Ok(role_arn)
}

use anyhow::{Context, Result};
use futures::future::join_all;

use crate::config::Config;

use super::infra::{
    create_schedule_with_retry, ensure_schedule_group, sanitize_explicit_name,
    sanitize_schedule_name, update_schedule_with_retry, validate_role_arn,
    validate_schedule_expression, CreateOutcome, ScheduleParams,
};

/// Options for the create_schedule operation.
pub struct CreateScheduleOpts<'a> {
    pub name: &'a str,
    pub count: i32,
    pub schedule_expression: &'a str,
    pub timezone: &'a str,
    pub role_arn: &'a str,
    pub explicit_name: Option<&'a str>,
}

/// Create EventBridge Scheduler schedules for a service or @group.
///
/// Requires a user-provided `role_arn` — does NOT auto-create IAM roles.
pub async fn create_schedule(
    aws_config: &aws_config::SdkConfig,
    cfg: &Config,
    opts: &CreateScheduleOpts<'_>,
) -> Result<()> {
    if opts.count < 0 {
        anyhow::bail!("count must be >= 0, got {}", opts.count);
    }
    validate_schedule_expression(opts.schedule_expression)?;
    validate_role_arn(opts.role_arn)?;

    let targets = cfg.resolve_targets(opts.name);
    let group_name = cfg.scheduler_group_name().to_string();

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", opts.name);
    }

    let scheduler = aws_sdk_scheduler::Client::new(aws_config);
    ensure_schedule_group(&scheduler, &group_name).await?;

    for alias in &targets {
        let (cluster, service) = cfg.resolve_alias(alias)?;

        let schedule_name = match opts.explicit_name {
            Some(n) => {
                let raw = if targets.len() == 1 {
                    n.to_string()
                } else {
                    format!("{}-{}", n, alias)
                };
                // Sanitize and enforce 64-char limit for explicit names.
                // Uses same hash strategy as auto-generated names for
                // collision resistance on truncation.
                sanitize_explicit_name(&raw)
            }
            None => sanitize_schedule_name(alias, opts.count),
        };
        let description = format!(
            "ecsctl: scale {} ({}/{}) to {}",
            alias, cluster, service, opts.count
        );

        let target_input = serde_json::json!({
            "Cluster": cluster,
            "Service": service,
            "DesiredCount": opts.count
        });

        let target = aws_sdk_scheduler::types::Target::builder()
            .arn("arn:aws:scheduler:::aws-sdk:ecs:updateService")
            .role_arn(opts.role_arn)
            .input(target_input.to_string())
            .build()?;

        let ftw = aws_sdk_scheduler::types::FlexibleTimeWindow::builder()
            .mode(aws_sdk_scheduler::types::FlexibleTimeWindowMode::Off)
            .build()?;

        let params = ScheduleParams {
            schedule_name: &schedule_name,
            group_name: &group_name,
            schedule_expression: opts.schedule_expression,
            timezone: opts.timezone,
            ftw,
            target,
            description: &description,
        };

        // Optimistic create — if schedule already exists, fall back to update.
        // ConflictException is handled via typed SDK error (no string matching).
        match create_schedule_with_retry(&scheduler, &params).await? {
            CreateOutcome::Created => {
                eprintln!("✓ Created: {schedule_name}");
            }
            CreateOutcome::AlreadyExists => {
                eprintln!(
                    "⚠️  Schedule '{schedule_name}' already exists — updating with new parameters"
                );
                update_schedule_with_retry(&scheduler, &params).await?;
                eprintln!("✓ Updated: {schedule_name}");
            }
        }

        eprintln!(
            "  Expression: {} ({})",
            opts.schedule_expression, opts.timezone
        );
        eprintln!("  Action:     scale {alias} ({service}) to {}", opts.count);
    }

    eprintln!("\n  Use 'ecsctl schedule list' to view all schedules");
    Ok(())
}

/// List all schedules in the configured schedule group.
///
/// Fetches schedule details concurrently to avoid sequential N+1 latency.
pub async fn list_schedules(aws_config: &aws_config::SdkConfig, cfg: &Config) -> Result<()> {
    let group_name = cfg.scheduler_group_name().to_string();
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);

    let mut all = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let mut req = scheduler.list_schedules().group_name(&group_name);
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

    // Fetch details concurrently to avoid sequential N+1 latency.
    // Scale is bounded by fleet size (typically 5-20 schedules).
    let detail_futures: Vec<_> = all
        .iter()
        .map(|s| {
            let name = s.name().unwrap_or("-").to_string();
            let client = scheduler.clone();
            let gn = group_name.clone();
            async move {
                let detail = client
                    .get_schedule()
                    .name(&name)
                    .group_name(&gn)
                    .send()
                    .await;
                (name, detail)
            }
        })
        .collect();

    let details = join_all(detail_futures).await;

    println!(
        "{:<40} {:<28} {:<14} {:<8} TARGET",
        "NAME", "SCHEDULE", "TIMEZONE", "STATE"
    );
    for (idx, (name, detail_result)) in details.iter().enumerate() {
        let state = all[idx].state().map(|st| st.as_str()).unwrap_or("?");

        let (expr, tz, target_info) = match detail_result {
            Ok(detail) => {
                let expr = detail.schedule_expression().unwrap_or("-").to_string();
                let tz = detail
                    .schedule_expression_timezone()
                    .unwrap_or("UTC")
                    .to_string();
                let target_info = detail
                    .target()
                    .and_then(|t| t.input())
                    .and_then(parse_target_display)
                    .unwrap_or_else(|| "-".to_string());
                (expr, tz, target_info)
            }
            Err(_) => ("-".to_string(), "-".to_string(), "-".to_string()),
        };
        println!(
            "{:<40} {:<28} {:<14} {:<8} {}",
            name, expr, tz, state, target_info
        );
    }
    Ok(())
}

/// Parse the target input JSON to produce a human-readable display string.
fn parse_target_display(input: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let cluster = v.get("Cluster")?.as_str()?;
    let service = v.get("Service")?.as_str()?;
    let count = v.get("DesiredCount")?.as_i64()?;
    Some(format!("{}/{} → {}", cluster, service, count))
}

/// Delete a schedule by name.
pub async fn delete_schedule(
    aws_config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("schedule name cannot be empty");
    }
    let group_name = cfg.scheduler_group_name().to_string();
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);

    match scheduler
        .delete_schedule()
        .name(name)
        .group_name(&group_name)
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

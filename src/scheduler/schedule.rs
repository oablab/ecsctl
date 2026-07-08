use anyhow::{Context, Result};

use crate::config::Config;

use super::infra::{
    create_or_update_schedule_with_retry, ensure_schedule_group, sanitize_schedule_name,
    schedule_exists, validate_schedule_expression,
};

/// Create EventBridge Scheduler schedules for a service or @group.
///
/// Requires a user-provided `role_arn` — does NOT auto-create IAM roles.
pub async fn create_schedule(
    aws_config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    count: i32,
    schedule_expression: &str,
    timezone: &str,
    role_arn: &str,
) -> Result<()> {
    if count < 0 {
        anyhow::bail!("count must be >= 0, got {count}");
    }
    validate_schedule_expression(schedule_expression)?;

    let targets = cfg.resolve_targets(name);

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", name);
    }

    let group_name = cfg.scheduler_group_name();
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);
    ensure_schedule_group(&scheduler, group_name).await?;

    for alias in &targets {
        let (cluster, service) = cfg.resolve_alias(alias)?;

        let schedule_name = sanitize_schedule_name(alias, count);

        let target_input = serde_json::json!({
            "Cluster": cluster,
            "Service": service,
            "DesiredCount": count
        });

        let target = aws_sdk_scheduler::types::Target::builder()
            .arn("arn:aws:scheduler:::aws-sdk:ecs:updateService")
            .role_arn(role_arn)
            .input(target_input.to_string())
            .build()?;

        let ftw = aws_sdk_scheduler::types::FlexibleTimeWindow::builder()
            .mode(aws_sdk_scheduler::types::FlexibleTimeWindowMode::Off)
            .build()?;

        let exists = schedule_exists(&scheduler, &schedule_name, group_name).await?;

        create_or_update_schedule_with_retry(
            &scheduler,
            &schedule_name,
            group_name,
            schedule_expression,
            timezone,
            ftw,
            target,
            exists,
        )
        .await?;

        if exists {
            eprintln!("✓ Updated: {schedule_name}");
        } else {
            eprintln!("✓ Created: {schedule_name}");
        }

        eprintln!("  Expression: {schedule_expression} ({timezone})");
        eprintln!("  Action:     scale {alias} ({service}) to {count}");
    }

    eprintln!("\n  Use 'ecsctl schedule list' to view all schedules");
    Ok(())
}

/// List all schedules in the ecsctl schedule group.
pub async fn list_schedules(aws_config: &aws_config::SdkConfig, cfg: &Config) -> Result<()> {
    let group_name = cfg.scheduler_group_name();
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);

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
pub async fn delete_schedule(
    aws_config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
) -> Result<()> {
    let group_name = cfg.scheduler_group_name();
    let scheduler = aws_sdk_scheduler::Client::new(aws_config);

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

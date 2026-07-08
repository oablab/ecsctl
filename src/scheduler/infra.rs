use anyhow::{Context, Result};
use aws_sdk_scheduler::error::ProvideErrorMetadata;

/// Ensure the schedule group exists, creating it if not found.
pub async fn ensure_schedule_group(
    scheduler: &aws_sdk_scheduler::Client,
    group_name: &str,
) -> Result<()> {
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
        }
    }

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
                Ok(())
            } else {
                Err(e).context("failed to create schedule group")
            }
        }
    }
}

/// Validate a schedule expression (cron, rate, or at).
pub fn validate_schedule_expression(expr: &str) -> Result<()> {
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

/// Sanitize an alias name for use in a schedule name.
pub fn sanitize_schedule_name(alias: &str, count: i32) -> String {
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
    format!("{}{}{}", prefix, truncated, suffix)
}

/// Check if a schedule already exists.
pub async fn schedule_exists(
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

/// Determine if an SDK error is a retryable IAM propagation issue.
///
/// Checks for ValidationException with messages indicating the scheduler
/// cannot assume the provided role (IAM eventual consistency).
fn is_retryable_iam_error(code: &str, message: &str) -> bool {
    code == "ValidationException"
        && (message.contains("unable to assume") || message.contains("cannot be assumed"))
}

/// Parameters for creating or updating a schedule.
pub struct ScheduleParams<'a> {
    pub schedule_name: &'a str,
    pub group_name: &'a str,
    pub schedule_expression: &'a str,
    pub timezone: &'a str,
    pub ftw: aws_sdk_scheduler::types::FlexibleTimeWindow,
    pub target: aws_sdk_scheduler::types::Target,
    pub is_update: bool,
}

/// Create or update a schedule with retry + exponential backoff to handle IAM propagation delay.
///
/// Both create and update can fail if the IAM role hasn't propagated yet,
/// so we use a unified retry for both operations.
pub async fn create_or_update_schedule_with_retry(
    scheduler: &aws_sdk_scheduler::Client,
    params: ScheduleParams<'_>,
) -> Result<()> {
    let max_attempts = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let ftw_clone = params.ftw.clone();
        let target_clone = params.target.clone();

        // Execute the appropriate operation and extract retryability info
        let (success, is_retryable, err_context) = if params.is_update {
            match scheduler
                .update_schedule()
                .name(params.schedule_name)
                .group_name(params.group_name)
                .schedule_expression(params.schedule_expression)
                .schedule_expression_timezone(params.timezone)
                .flexible_time_window(ftw_clone)
                .target(target_clone)
                .send()
                .await
            {
                Ok(_) => (true, false, None),
                Err(e) => {
                    let retryable = e
                        .as_service_error()
                        .map(|se| {
                            is_retryable_iam_error(
                                se.code().unwrap_or(""),
                                se.message().unwrap_or(""),
                            )
                        })
                        .unwrap_or(false);
                    (false, retryable, Some(format!("{e}")))
                }
            }
        } else {
            match scheduler
                .create_schedule()
                .name(params.schedule_name)
                .group_name(params.group_name)
                .schedule_expression(params.schedule_expression)
                .schedule_expression_timezone(params.timezone)
                .flexible_time_window(ftw_clone)
                .target(target_clone)
                .send()
                .await
            {
                Ok(_) => (true, false, None),
                Err(e) => {
                    let retryable = e
                        .as_service_error()
                        .map(|se| {
                            is_retryable_iam_error(
                                se.code().unwrap_or(""),
                                se.message().unwrap_or(""),
                            )
                        })
                        .unwrap_or(false);
                    (false, retryable, Some(format!("{e}")))
                }
            }
        };

        if success {
            return Ok(());
        }

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
            let op = if params.is_update { "update" } else { "create" };
            anyhow::bail!(
                "failed to {op} schedule '{}': {}",
                params.schedule_name,
                err_context.unwrap_or_else(|| "unknown error".to_string())
            );
        }
    }
}

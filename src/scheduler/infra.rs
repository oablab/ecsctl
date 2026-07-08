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
///
/// Produces deterministic names in the form: `ecsctl-scale-{alias}-to-{count}`.
/// When truncation is needed (64-char limit), appends a short hash of the full alias
/// to prevent collisions between aliases that share a common prefix.
pub fn sanitize_schedule_name(alias: &str, count: i32) -> String {
    let safe_alias = alias.replace(
        |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '.',
        "-",
    );
    let suffix = format!("-to-{}", count);
    let prefix = "ecsctl-scale-";

    let max_alias_len = 64 - prefix.len() - suffix.len();
    if safe_alias.len() <= max_alias_len {
        format!("{}{}{}", prefix, safe_alias, suffix)
    } else {
        // When truncating, append a 6-char hash to prevent collisions.
        // Hash the ORIGINAL alias (not safe_alias) to also distinguish aliases
        // that differ only in characters normalized to '-'.
        let hash = simple_hash(alias);
        let hash_suffix = format!("-{:06x}", hash & 0xFFFFFF);
        let truncated_max = max_alias_len - hash_suffix.len();
        let truncated = &safe_alias[..truncated_max];
        format!("{}{}{}{}", prefix, truncated, hash_suffix, suffix)
    }
}

/// Simple deterministic hash (FNV-1a) for collision prevention.
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

/// Create a schedule with retry + exponential backoff to handle IAM propagation delay.
pub async fn create_schedule_with_retry(
    scheduler: &aws_sdk_scheduler::Client,
    schedule_name: &str,
    group_name: &str,
    schedule_expression: &str,
    timezone: &str,
    ftw: aws_sdk_scheduler::types::FlexibleTimeWindow,
    target: aws_sdk_scheduler::types::Target,
    description: &str,
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
            .description(description)
            .send()
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                let is_retryable = e
                    .as_service_error()
                    .map(|se| {
                        let code = se.code().unwrap_or("");
                        let msg = se.message().unwrap_or("");
                        // Retry on IAM propagation delay
                        let iam_not_ready =
                            msg.contains("role") || msg.contains("unable to assume");
                        // Retry on throttling
                        let throttled =
                            code == "ThrottlingException" || code == "TooManyRequestsException";
                        iam_not_ready || throttled
                    })
                    .unwrap_or(false);

                if is_retryable && attempt < max_attempts {
                    let delay = std::time::Duration::from_secs(5 * attempt as u64);
                    eprintln!(
                        "  ⏳ Retryable error, retrying in {}s (attempt {}/{})",
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_schedule_expression tests ---

    #[test]
    fn test_valid_cron_expression() {
        assert!(validate_schedule_expression("cron(0 8 * * ? *)").is_ok());
        assert!(validate_schedule_expression("cron(30 22 * * ? 2024)").is_ok());
        assert!(validate_schedule_expression("cron(0 0 1 1 ? *)").is_ok());
    }

    #[test]
    fn test_cron_wrong_field_count() {
        let err = validate_schedule_expression("cron(0 8 * * *)").unwrap_err();
        assert!(err.to_string().contains("expected 6 fields"));

        let err = validate_schedule_expression("cron(0 8 * * ? * extra)").unwrap_err();
        assert!(err.to_string().contains("expected 6 fields"));
    }

    #[test]
    fn test_valid_rate_expression() {
        assert!(validate_schedule_expression("rate(5 minutes)").is_ok());
        assert!(validate_schedule_expression("rate(1 hour)").is_ok());
        assert!(validate_schedule_expression("rate(7 days)").is_ok());
        assert!(validate_schedule_expression("rate(1 minute)").is_ok());
    }

    #[test]
    fn test_rate_empty() {
        let err = validate_schedule_expression("rate()").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_rate_invalid_value() {
        let err = validate_schedule_expression("rate(abc minutes)").unwrap_err();
        assert!(err.to_string().contains("not a positive integer"));
    }

    #[test]
    fn test_rate_invalid_unit() {
        let err = validate_schedule_expression("rate(5 weeks)").unwrap_err();
        assert!(err.to_string().contains("not recognized"));
    }

    #[test]
    fn test_rate_wrong_format() {
        let err = validate_schedule_expression("rate(5)").unwrap_err();
        assert!(err.to_string().contains("expected 'rate(<value> <unit>)'"));
    }

    #[test]
    fn test_valid_at_expression() {
        assert!(validate_schedule_expression("at(2024-01-01T00:00:00)").is_ok());
        assert!(validate_schedule_expression("at(2026-12-31T23:59:59)").is_ok());
    }

    #[test]
    fn test_at_empty() {
        let err = validate_schedule_expression("at()").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_invalid_expression_format() {
        let err = validate_schedule_expression("every 5 minutes").unwrap_err();
        assert!(err.to_string().contains("Must start with"));

        let err = validate_schedule_expression("").unwrap_err();
        assert!(err.to_string().contains("Must start with"));
    }

    // --- sanitize_schedule_name tests ---

    #[test]
    fn test_sanitize_normal_name() {
        let name = sanitize_schedule_name("chaodu", 0);
        assert_eq!(name, "ecsctl-scale-chaodu-to-0");

        let name = sanitize_schedule_name("my-bot", 1);
        assert_eq!(name, "ecsctl-scale-my-bot-to-1");
    }

    #[test]
    fn test_sanitize_special_characters() {
        let name = sanitize_schedule_name("bot@special!name", 2);
        assert_eq!(name, "ecsctl-scale-bot-special-name-to-2");
    }

    #[test]
    fn test_sanitize_long_name_truncated_with_hash() {
        let long_alias = "a".repeat(100);
        let name = sanitize_schedule_name(&long_alias, 0);
        // Must fit in 64 chars
        assert!(name.len() <= 64, "name too long: {} chars", name.len());
        // Must contain standard prefix/suffix
        assert!(name.starts_with("ecsctl-scale-"));
        assert!(name.ends_with("-to-0"));
    }

    #[test]
    fn test_sanitize_truncation_different_aliases_no_collision() {
        // Two long aliases with same prefix but different endings
        // should produce different schedule names due to hash suffix
        let alias1 = format!("{}{}", "a".repeat(50), "xxx");
        let alias2 = format!("{}{}", "a".repeat(50), "yyy");
        let name1 = sanitize_schedule_name(&alias1, 0);
        let name2 = sanitize_schedule_name(&alias2, 0);
        assert_ne!(
            name1, name2,
            "truncated aliases should not collide: {name1} vs {name2}"
        );
    }

    #[test]
    fn test_sanitize_deterministic() {
        let name1 = sanitize_schedule_name("test-alias", 5);
        let name2 = sanitize_schedule_name("test-alias", 5);
        assert_eq!(name1, name2);
    }

    #[test]
    fn test_sanitize_normalization_collision_prevention() {
        // Aliases that differ only in characters normalized to '-' should still
        // produce different schedule names when truncation activates the hash.
        let alias1 = format!("{}", "a/b".repeat(30)); // contains '/' → '-'
        let alias2 = format!("{}", "a-b".repeat(30)); // already '-'
        let name1 = sanitize_schedule_name(&alias1, 0);
        let name2 = sanitize_schedule_name(&alias2, 0);
        // Both are long enough to trigger truncation+hash
        assert!(name1.len() <= 64);
        assert!(name2.len() <= 64);
        assert_ne!(
            name1, name2,
            "aliases differing only in normalized chars should not collide"
        );
    }

    #[test]
    fn test_sanitize_short_names_no_hash() {
        // Short names should be used as-is without hash suffix
        let name = sanitize_schedule_name("web", 0);
        assert_eq!(name, "ecsctl-scale-web-to-0");
        // No hash pattern in short names
        assert!(!name.contains("-0") || name == "ecsctl-scale-web-to-0");
    }
}

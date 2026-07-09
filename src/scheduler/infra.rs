use anyhow::{Context, Result};

/// Validate that a role ARN has the correct format for an IAM role.
///
/// Expected format: `arn:aws:iam::<account-id>:role/<role-name>`
/// This catches typos and wrong resource types (e.g. user ARN, policy ARN)
/// before making API calls that would fail with opaque errors.
pub fn validate_role_arn(role_arn: &str) -> Result<()> {
    let parts: Vec<&str> = role_arn.splitn(6, ':').collect();
    if parts.len() != 6 {
        anyhow::bail!(
            "invalid role ARN format: expected 'arn:aws:iam::<account-id>:role/<name>', got '{role_arn}'"
        );
    }
    let prefix = parts[0];
    let partition = parts[1];
    let service = parts[2];
    // parts[3] is region (empty for IAM)
    let account = parts[4];
    let resource = parts[5];

    if prefix != "arn" {
        anyhow::bail!("invalid role ARN: must start with 'arn:', got '{role_arn}'");
    }
    if !["aws", "aws-cn", "aws-us-gov", "aws-iso", "aws-iso-b"].contains(&partition) {
        anyhow::bail!(
            "invalid role ARN: unrecognized partition '{partition}'. Expected aws, aws-cn, aws-us-gov, aws-iso, or aws-iso-b"
        );
    }
    if service != "iam" {
        anyhow::bail!(
            "invalid role ARN: service must be 'iam', got '{service}'. \
             Make sure you're passing an IAM role ARN, not a {service} ARN"
        );
    }
    if account.is_empty() || !account.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("invalid role ARN: account ID must be numeric, got '{account}'");
    }
    if !resource.starts_with("role/") {
        anyhow::bail!(
            "invalid role ARN: resource must start with 'role/', got '{resource}'. \
             Make sure you're passing an IAM role ARN, not a user or policy ARN"
        );
    }
    let role_name = &resource[5..];
    if role_name.is_empty() {
        anyhow::bail!("invalid role ARN: role name cannot be empty");
    }
    Ok(())
}

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
/// Produces deterministic names in the form: `ecsctl-scale-{safe_alias}-to-{count}`.
/// Appends a short hash suffix when:
/// - The alias was normalized (contains characters replaced by '-'), to prevent
///   collisions between aliases like `web/api` and `web-api`.
/// - The name exceeds the 64-char limit and must be truncated.
pub fn sanitize_schedule_name(alias: &str, count: i32) -> String {
    let safe_alias = alias.replace(
        |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '.',
        "-",
    );
    let suffix = format!("-to-{}", count);
    let prefix = "ecsctl-scale-";

    let needs_hash = safe_alias != alias;
    let max_alias_len = 64 - prefix.len() - suffix.len();

    if !needs_hash && safe_alias.len() <= max_alias_len {
        // No normalization, no truncation — use as-is.
        format!("{}{}{}", prefix, safe_alias, suffix)
    } else {
        // Append a 6-char hash to prevent collisions from normalization or truncation.
        // Hash the ORIGINAL alias to distinguish aliases that differ only in
        // characters normalized to '-'.
        let hash = simple_hash(alias);
        let hash_suffix = format!("-{:06x}", hash & 0xFFFFFF);
        let available = max_alias_len - hash_suffix.len();
        let base = if safe_alias.len() <= available {
            &safe_alias[..]
        } else {
            &safe_alias[..available]
        };
        format!("{}{}{}{}", prefix, base, hash_suffix, suffix)
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

/// Sanitize an explicit schedule name (from `--schedule-name`).
///
/// Applies the same collision-resistant strategy as `sanitize_schedule_name`:
/// appends a FNV-1a hash suffix when normalization changes the name or
/// truncation is needed.
pub fn sanitize_explicit_name(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let needs_hash = sanitized != raw;

    if !needs_hash && sanitized.len() <= 64 {
        sanitized
    } else {
        let hash = simple_hash(raw);
        let hash_suffix = format!("-{:06x}", hash & 0xFFFFFF);
        let available = 64 - hash_suffix.len();
        let base = if sanitized.len() <= available {
            &sanitized[..]
        } else {
            &sanitized[..available]
        };
        format!("{}{}", base, hash_suffix)
    }
}

/// Parameters for creating or updating a schedule.
pub(crate) struct ScheduleParams<'a> {
    pub(crate) schedule_name: &'a str,
    pub(crate) group_name: &'a str,
    pub(crate) schedule_expression: &'a str,
    pub(crate) timezone: &'a str,
    pub(crate) ftw: aws_sdk_scheduler::types::FlexibleTimeWindow,
    pub(crate) target: aws_sdk_scheduler::types::Target,
    pub(crate) description: &'a str,
}

/// Outcome of a schedule creation attempt.
pub enum CreateOutcome {
    /// Schedule was created successfully.
    Created,
    /// Schedule already exists (ConflictException).
    AlreadyExists,
}

/// Create a schedule with retry + exponential backoff to handle IAM propagation delay.
///
/// Returns `Ok(CreateOutcome::AlreadyExists)` if the schedule already exists
/// (ConflictException), allowing the caller to fall back to update without
/// relying on fragile string matching.
pub async fn create_schedule_with_retry(
    scheduler: &aws_sdk_scheduler::Client,
    params: &ScheduleParams<'_>,
) -> Result<CreateOutcome> {
    let max_attempts = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match scheduler
            .create_schedule()
            .name(params.schedule_name)
            .group_name(params.group_name)
            .schedule_expression(params.schedule_expression)
            .schedule_expression_timezone(params.timezone)
            .flexible_time_window(params.ftw.clone())
            .target(params.target.clone())
            .description(params.description)
            .send()
            .await
        {
            Ok(_) => return Ok(CreateOutcome::Created),
            Err(e) => {
                // ConflictException means schedule already exists — not an error
                if e.as_service_error()
                    .map(|se| se.is_conflict_exception())
                    .unwrap_or(false)
                {
                    return Ok(CreateOutcome::AlreadyExists);
                }
                if is_retryable_scheduler_error(&e) && attempt < max_attempts {
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

/// Update a schedule with retry + exponential backoff to handle IAM propagation delay.
pub async fn update_schedule_with_retry(
    scheduler: &aws_sdk_scheduler::Client,
    params: &ScheduleParams<'_>,
) -> Result<()> {
    schedule_op_with_retry("update", || async {
        scheduler
            .update_schedule()
            .name(params.schedule_name)
            .group_name(params.group_name)
            .schedule_expression(params.schedule_expression)
            .schedule_expression_timezone(params.timezone)
            .flexible_time_window(params.ftw.clone())
            .target(params.target.clone())
            .description(params.description)
            .send()
            .await
            .map(|_| ())
    })
    .await
}

/// Generic retry loop for schedule operations with exponential backoff.
///
/// Retries on IAM propagation errors and throttling (max 4 attempts: 1 initial + 3 retries at 5s/10s/15s).
async fn schedule_op_with_retry<F, Fut, E>(op_name: &str, op: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), aws_sdk_scheduler::error::SdkError<E>>>,
    E: aws_sdk_scheduler::error::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let max_attempts = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if is_retryable_scheduler_error(&e) && attempt < max_attempts {
                    let delay = std::time::Duration::from_secs(5 * attempt as u64);
                    eprintln!(
                        "  ⏳ Retryable error, retrying in {}s (attempt {}/{})",
                        delay.as_secs(),
                        attempt,
                        max_attempts
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    return Err(e).context(format!("failed to {op_name} schedule"));
                }
            }
        }
    }
}

/// Check whether an SDK error is retryable (IAM propagation or throttling).
fn is_retryable_scheduler_error<E: aws_sdk_scheduler::error::ProvideErrorMetadata>(
    e: &aws_sdk_scheduler::error::SdkError<E>,
) -> bool {
    e.as_service_error()
        .map(|se| {
            let code = se.code().unwrap_or("");
            let msg = se.message().unwrap_or("").to_lowercase();
            // Retry only on IAM propagation delay (role exists but not yet
            // visible to the scheduler service). Permanent errors like
            // "role ARN is invalid" or "not authorized to pass role" should
            // NOT be retried.
            let iam_not_ready = msg.contains("unable to assume")
                || msg.contains("cannot be assumed")
                || msg.contains("is not yet ready");
            // Retry on throttling
            let throttled = code == "ThrottlingException" || code == "TooManyRequestsException";
            iam_not_ready || throttled
        })
        .unwrap_or(false)
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
        // Contains normalized chars → hash appended
        assert!(name.starts_with("ecsctl-scale-bot-special-name-"));
        assert!(name.ends_with("-to-2"));
        assert!(name.len() <= 64);
        // Different aliases that normalize the same should NOT collide
        let name2 = sanitize_schedule_name("bot-special-name", 2);
        assert_ne!(name, name2, "normalized aliases should not collide");
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
        // produce different schedule names — both short and long.

        // Short aliases (no truncation, but normalization triggers hash)
        let short1 = "web/api"; // '/' → '-'
        let short2 = "web-api"; // already '-'
        let name1 = sanitize_schedule_name(short1, 0);
        let name2 = sanitize_schedule_name(short2, 0);
        assert_ne!(
            name1, name2,
            "short aliases differing only in normalized chars should not collide"
        );
        // The non-normalized one should NOT have a hash
        assert_eq!(name2, "ecsctl-scale-web-api-to-0");
        // The normalized one SHOULD have a hash
        assert!(name1.contains("-") && name1 != "ecsctl-scale-web-api-to-0");

        // Long aliases (truncation + hash)
        let long1 = "a/b".repeat(30); // contains '/' → '-'
        let long2 = "a-b".repeat(30); // already '-'
        let name1 = sanitize_schedule_name(&long1, 0);
        let name2 = sanitize_schedule_name(&long2, 0);
        assert!(name1.len() <= 64);
        assert!(name2.len() <= 64);
        assert_ne!(
            name1, name2,
            "long aliases differing only in normalized chars should not collide"
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

    // --- validate_role_arn tests ---

    #[test]
    fn test_valid_role_arn() {
        assert!(validate_role_arn("arn:aws:iam::123456789012:role/ecsctl-scheduler-role").is_ok());
        assert!(validate_role_arn("arn:aws:iam::123456789012:role/path/to/role").is_ok());
        assert!(validate_role_arn("arn:aws-cn:iam::123456789012:role/my-role").is_ok());
        assert!(validate_role_arn("arn:aws-us-gov:iam::123456789012:role/gov-role").is_ok());
        assert!(validate_role_arn("arn:aws-iso:iam::123456789012:role/iso-role").is_ok());
        assert!(validate_role_arn("arn:aws-iso-b:iam::123456789012:role/isob-role").is_ok());
    }

    #[test]
    fn test_role_arn_wrong_service() {
        let err = validate_role_arn("arn:aws:ecs::123456789012:service/my-svc").unwrap_err();
        assert!(err.to_string().contains("service must be 'iam'"));
    }

    #[test]
    fn test_role_arn_wrong_resource_type() {
        let err = validate_role_arn("arn:aws:iam::123456789012:user/admin").unwrap_err();
        assert!(err.to_string().contains("must start with 'role/'"));
    }

    #[test]
    fn test_role_arn_invalid_account() {
        let err = validate_role_arn("arn:aws:iam::not-a-number:role/my-role").unwrap_err();
        assert!(err.to_string().contains("account ID must be numeric"));
    }

    #[test]
    fn test_role_arn_malformed() {
        let err = validate_role_arn("not-an-arn").unwrap_err();
        assert!(err.to_string().contains("invalid role ARN format"));
    }

    #[test]
    fn test_role_arn_empty_role_name() {
        let err = validate_role_arn("arn:aws:iam::123456789012:role/").unwrap_err();
        assert!(err.to_string().contains("role name cannot be empty"));
    }

    #[test]
    fn test_role_arn_policy_arn_rejected() {
        let err = validate_role_arn("arn:aws:iam::123456789012:policy/my-policy").unwrap_err();
        assert!(err.to_string().contains("must start with 'role/'"));
    }

    #[test]
    fn test_role_arn_wrong_prefix() {
        // 6-part ARN-like string but prefix is not "arn"
        let err = validate_role_arn("notarn:aws:iam::123456789012:role/my-role").unwrap_err();
        assert!(err.to_string().contains("must start with 'arn:'"));
    }

    #[test]
    fn test_role_arn_empty_account() {
        let err = validate_role_arn("arn:aws:iam:::role/my-role").unwrap_err();
        assert!(err.to_string().contains("account ID must be numeric"));
    }
}

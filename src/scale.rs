use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

fn validate_count(count: i32) -> Result<()> {
    if count < 0 {
        anyhow::bail!("count must be >= 0, got {count}");
    }
    Ok(())
}

/// Core scaling logic — takes an EcsClient directly for testability.
pub async fn scale_service(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    count: i32,
    wait: bool,
) -> Result<()> {
    eprintln!("⚖️  Scaling {service} to {count}...");
    ecs.update_service()
        .cluster(cluster)
        .service(service)
        .desired_count(count)
        .send()
        .await
        .context("UpdateService (scale) failed")?;

    eprintln!("✓ Desired count set to {count} for {cluster}/{service}");

    if wait {
        eprintln!("⏳ Waiting for deployment to stabilize...");
        crate::apply::wait_for_stable(ecs, cluster, service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

pub async fn run(config: &aws_config::SdkConfig, name: &str, count: i32, wait: bool) -> Result<()> {
    validate_count(count)?;

    let cfg = Config::load()?;
    let target = cfg
        .aliases
        .get(name)
        .context(format!("alias '{name}' not found"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2..=4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target"),
    };

    let ecs = EcsClient::new(config);
    scale_service(&ecs, cluster, service, count, wait).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ecs::operation::update_service::UpdateServiceOutput;
    use aws_smithy_mocks::{mock, mock_client, RuleMode};

    #[test]
    fn test_validate_count_negative() {
        assert!(validate_count(-1).is_err());
        assert!(validate_count(-100).is_err());
    }

    #[test]
    fn test_validate_count_zero() {
        assert!(validate_count(0).is_ok());
    }

    #[test]
    fn test_validate_count_positive() {
        assert!(validate_count(1).is_ok());
        assert!(validate_count(10).is_ok());
    }

    #[tokio::test]
    async fn test_scale_calls_update_service_with_correct_params() {
        let update_rule = mock!(aws_sdk_ecs::Client::update_service)
            .match_requests(|req| {
                req.cluster() == Some("test-cluster")
                    && req.service() == Some("test-service")
                    && req.desired_count() == Some(3)
            })
            .then_output(|| UpdateServiceOutput::builder().build());

        let ecs = mock_client!(aws_sdk_ecs, RuleMode::MatchAny, [&update_rule]);

        scale_service(&ecs, "test-cluster", "test-service", 3, false)
            .await
            .unwrap();

        assert_eq!(update_rule.num_calls(), 1);
    }

    #[tokio::test]
    async fn test_scale_to_zero() {
        let update_rule = mock!(aws_sdk_ecs::Client::update_service)
            .match_requests(|req| req.desired_count() == Some(0))
            .then_output(|| UpdateServiceOutput::builder().build());

        let ecs = mock_client!(aws_sdk_ecs, RuleMode::MatchAny, [&update_rule]);

        scale_service(&ecs, "my-cluster", "my-service", 0, false)
            .await
            .unwrap();

        assert_eq!(update_rule.num_calls(), 1);
    }
}

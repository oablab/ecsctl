use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

fn validate_count(count: i32) -> Result<()> {
    if count < 0 {
        anyhow::bail!("count must be >= 0, got {count}");
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
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

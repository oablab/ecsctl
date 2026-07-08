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
        let (cluster, service) = cfg.resolve_alias(alias)?;
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
        let (cluster, service) = cfg.resolve_alias(alias)?;
        eprintln!("⏳ Waiting for service to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Service stable");
    }

    Ok(())
}

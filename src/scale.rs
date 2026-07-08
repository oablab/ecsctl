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

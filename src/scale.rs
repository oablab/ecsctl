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
        let target = cfg
            .aliases
            .get(alias)
            .context(format!("alias '{alias}' not found"))?
            .clone();

        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = match parts.len() {
            2..=4 => (parts[0], parts[1]),
            _ => anyhow::bail!("invalid alias target for '{alias}'"),
        };

        ecs.update_service()
            .cluster(cluster)
            .service(service)
            .desired_count(count)
            .send()
            .await
            .context(format!("UpdateService failed for {alias}"))?;

        eprintln!("✓ {alias} → desired_count={count}");
    }

    if wait && targets.len() == 1 {
        let alias = &targets[0];
        let target = cfg.aliases.get(alias).unwrap();
        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = (parts[0], parts[1]);
        eprintln!("⏳ Waiting for service to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Service stable");
    }

    Ok(())
}

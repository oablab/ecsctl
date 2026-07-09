use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    wait: bool,
) -> Result<()> {
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

        eprintln!("🔄 Restarting {alias} ({service})...");
        ecs.update_service()
            .cluster(cluster)
            .service(service)
            .force_new_deployment(true)
            .send()
            .await
            .context(format!("UpdateService failed for {alias}"))?;

        eprintln!("✓ New deployment triggered for {alias}");
    }

    if wait && targets.len() == 1 {
        let alias = &targets[0];
        let target = cfg.aliases.get(alias).unwrap();
        let parts: Vec<&str> = target.splitn(4, '/').collect();
        let (cluster, service) = (parts[0], parts[1]);
        eprintln!("⏳ Waiting for deployment to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

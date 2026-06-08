use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(config: &aws_config::SdkConfig, name: &str, wait: bool) -> Result<()> {
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

    eprintln!("🔄 Restarting {service}...");
    ecs.update_service()
        .cluster(cluster)
        .service(service)
        .force_new_deployment(true)
        .send()
        .await
        .context("UpdateService (force new deployment) failed")?;

    eprintln!("✓ New deployment triggered for {cluster}/{service}");

    if wait {
        eprintln!("⏳ Waiting for deployment to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(config: &aws_config::SdkConfig, name: &str) -> Result<()> {
    let cfg = Config::load()?;
    let target = cfg
        .aliases
        .get(name)
        .context(format!("alias '{name}' not found — provide a known alias or service name"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2..=4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target"),
    };

    let ecs = EcsClient::new(config);

    // Scale to 0 first (required before delete)
    eprintln!("⏬ Scaling {service} to 0...");
    ecs.update_service()
        .cluster(cluster)
        .service(service)
        .desired_count(0)
        .send()
        .await
        .context("UpdateService (scale to 0) failed")?;

    // Delete service
    eprintln!("🗑️  Deleting service {service}...");
    ecs.delete_service()
        .cluster(cluster)
        .service(service)
        .send()
        .await
        .context("DeleteService failed")?;

    // Remove alias
    let mut cfg = Config::load()?;
    cfg.aliases.remove(name);
    cfg.save()?;

    eprintln!("✓ Deleted {cluster}/{service} and removed alias '{name}'");
    Ok(())
}

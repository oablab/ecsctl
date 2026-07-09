use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: Option<&str>,
    file: Option<&str>,
) -> Result<()> {
    let (cluster, service, alias_name) = match (name, file) {
        (Some(n), _) => resolve_from_alias(cfg, n)?,
        (None, Some(f)) => resolve_from_file(f).await?,
        _ => anyhow::bail!("provide a name or -f <file>"),
    };

    let ecs = EcsClient::new(config);

    // Scale to 0 first (required before delete)
    eprintln!("⏬ Scaling {service} to 0...");
    ecs.update_service()
        .cluster(&cluster)
        .service(&service)
        .desired_count(0)
        .send()
        .await
        .context("UpdateService (scale to 0) failed")?;

    // Delete service
    eprintln!("🗑️  Deleting service {service}...");
    ecs.delete_service()
        .cluster(&cluster)
        .service(&service)
        .send()
        .await
        .context("DeleteService failed")?;

    // Remove alias
    let mut cfg_mut = Config::load()?;
    cfg_mut.aliases.remove(&alias_name);
    cfg_mut.save()?;

    eprintln!("✓ Deleted {cluster}/{service}");
    Ok(())
}

fn resolve_from_alias(cfg: &Config, name: &str) -> Result<(String, String, String)> {
    let target = cfg
        .aliases
        .get(name)
        .context(format!("alias '{name}' not found"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    match parts.len() {
        2..=4 => Ok((parts[0].to_string(), parts[1].to_string(), name.to_string())),
        _ => anyhow::bail!("invalid alias target"),
    }
}

async fn resolve_from_file(file: &str) -> Result<(String, String, String)> {
    let content = crate::loader::load(file).await?;
    let spec: crate::apply::ServiceSpec =
        serde_yaml::from_str(&content).context("failed to parse spec")?;
    Ok((
        spec.metadata.cluster.clone(),
        spec.metadata.name.clone(),
        spec.metadata.name,
    ))
}

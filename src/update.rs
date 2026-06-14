use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

fn validate_overrides(overrides: &[String]) -> Result<()> {
    if overrides.is_empty() {
        anyhow::bail!("at least one --set override is required");
    }
    for entry in overrides {
        let key = entry.split('=').next().unwrap_or("");
        if key == "metadata.name" || key == "metadata.cluster" {
            anyhow::bail!("cannot override '{key}' via update — use 'clone' to deploy to a different service/cluster");
        }
    }
    Ok(())
}

pub async fn run(config: &aws_config::SdkConfig, name: &str, overrides: &[String], wait: bool) -> Result<()> {
    validate_overrides(overrides)?;

    // Guard: abort if service has sidecar containers that would be silently dropped
    check_no_sidecars(config, name).await?;

    eprintln!("📥 Exporting current state of '{name}'...");
    let yaml = crate::export::export_to_yaml(config, name).await?;
    crate::apply::run_from_string(config, &yaml, overrides, wait).await
}

async fn check_no_sidecars(config: &aws_config::SdkConfig, name: &str) -> Result<()> {
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

    let svc_resp = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed")?;

    let svc = svc_resp.services().first().context("service not found")?;
    let task_def_arn = svc.task_definition().context("no task definition")?;

    let td_resp = ecs
        .describe_task_definition()
        .task_definition(task_def_arn)
        .send()
        .await
        .context("DescribeTaskDefinition failed")?;

    let td = td_resp.task_definition().context("no task definition")?;

    let non_system: Vec<_> = td
        .container_definitions()
        .iter()
        .filter(|c| !c.name().unwrap_or_default().starts_with("ecs-service-connect-"))
        .collect();

    if non_system.len() > 1 {
        let names: Vec<_> = non_system.iter().map(|c| c.name().unwrap_or("?")).collect();
        anyhow::bail!(
            "service has {} containers ({}); update only supports single-container services to avoid silently dropping sidecars",
            non_system.len(),
            names.join(", ")
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_overrides_rejected() {
        let overrides: Vec<String> = vec![];
        assert!(validate_overrides(&overrides).is_err());
    }

    #[test]
    fn test_metadata_name_blocked() {
        let overrides = vec!["metadata.name=other".to_string()];
        let err = validate_overrides(&overrides).unwrap_err();
        assert!(err.to_string().contains("metadata.name"));
    }

    #[test]
    fn test_metadata_cluster_blocked() {
        let overrides = vec!["metadata.cluster=other".to_string()];
        let err = validate_overrides(&overrides).unwrap_err();
        assert!(err.to_string().contains("metadata.cluster"));
    }

    #[test]
    fn test_valid_overrides_accepted() {
        let overrides = vec!["spec.cpu=512".to_string()];
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn test_multiple_valid_overrides() {
        let overrides = vec![
            "spec.cpu=512".to_string(),
            "spec.image=nginx:latest".to_string(),
        ];
        assert!(validate_overrides(&overrides).is_ok());
    }
}

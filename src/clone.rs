use anyhow::Result;

/// Clone a service: export source → rename → apply as new service.
pub async fn run(
    config: &aws_config::SdkConfig,
    source: &str,
    target_name: &str,
    overrides: &[String],
) -> Result<()> {
    eprintln!("📋 Exporting {source}...");
    let yaml = crate::export::export_to_yaml(config, source).await?;

    // Always override metadata.name with the target name
    let mut all_overrides = vec![format!("metadata.name={target_name}")];
    all_overrides.extend(overrides.iter().cloned());

    eprintln!("🚀 Deploying as {target_name}...");
    crate::apply::run_from_string(config, &yaml, &all_overrides, false).await
}

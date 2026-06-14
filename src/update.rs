use anyhow::Result;

pub async fn run(config: &aws_config::SdkConfig, name: &str, overrides: &[String], wait: bool) -> Result<()> {
    eprintln!("📥 Exporting current state of '{name}'...");
    let yaml = crate::export::export_to_yaml(config, name).await?;
    crate::apply::run_from_string(config, &yaml, overrides, wait).await
}

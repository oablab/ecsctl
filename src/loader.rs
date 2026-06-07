use anyhow::{Context, Result};

/// Load content from a local file path or a remote URL (http/https).
pub async fn load(source: &str) -> Result<String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        reqwest::get(source)
            .await
            .context("failed to fetch remote URL")?
            .error_for_status()
            .context("remote URL returned error status")?
            .text()
            .await
            .context("failed to read response body")
    } else {
        std::fs::read_to_string(source).context("failed to read spec file")
    }
}

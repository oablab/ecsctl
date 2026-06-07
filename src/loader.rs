use anyhow::{Context, Result};

/// Returns true if the source looks like a remote URL.
pub fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Load content from a local file path or a remote URL (http/https).
pub async fn load(source: &str) -> Result<String> {
    if is_url(source) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_url_https() {
        assert!(is_url("https://example.com/service.yaml"));
    }

    #[test]
    fn test_is_url_http() {
        assert!(is_url("http://localhost:8080/spec.yaml"));
    }

    #[test]
    fn test_is_url_local_path() {
        assert!(!is_url("./service.yaml"));
        assert!(!is_url("/absolute/path/service.yaml"));
        assert!(!is_url("relative/service.yaml"));
    }

    #[tokio::test]
    async fn test_load_local_file() {
        let content = load("Cargo.toml").await.unwrap();
        assert!(content.contains("[package]"));
    }

    #[tokio::test]
    async fn test_load_missing_file() {
        let result = load("nonexistent.yaml").await;
        assert!(result.is_err());
    }
}

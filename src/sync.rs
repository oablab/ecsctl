use anyhow::{Context, Result};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_sts::Client as StsClient;
use std::process::Command;
use std::time::Duration;

/// Parse "cluster/task/container:/remote/path" into parts
fn parse_remote(s: &str) -> Result<(&str, &str, &str, &str)> {
    let colon_pos = s.find(':').context("remote must contain ':'")?;
    let remote_path = &s[colon_pos + 1..];
    let prefix = &s[..colon_pos];
    let parts: Vec<&str> = prefix.splitn(3, '/').collect();
    if parts.len() == 3 {
        Ok((parts[0], parts[1], parts[2], remote_path))
    } else {
        anyhow::bail!("remote must be cluster/task/container:/path")
    }
}

async fn get_staging_bucket(config: &aws_config::SdkConfig, bucket: Option<&str>) -> Result<String> {
    if let Some(b) = bucket {
        return Ok(b.to_string());
    }
    let sts = StsClient::new(config);
    let identity = sts.get_caller_identity().send().await?;
    let account_id = identity.account().context("no account ID")?;
    Ok(format!("ecsctl-staging-{account_id}"))
}

pub async fn run(
    config: &aws_config::SdkConfig,
    local_dir: &str,
    remote: &str,
    bucket: Option<&str>,
    presign_expiry_secs: u64,
) -> Result<()> {
    let (cluster, task, container, remote_path) = parse_remote(remote)?;
    let s3 = S3Client::new(config);
    let staging_bucket = get_staging_bucket(config, bucket).await?;
    let key = format!("ecsctl/{}.tar.gz", uuid::Uuid::new_v4());
    let expiry = Duration::from_secs(presign_expiry_secs);

    // 1. Tar up local directory
    eprintln!("📦 Compressing {local_dir}...");
    let tar_bytes = tar_dir(local_dir)?;
    eprintln!("   {:.1} KB", tar_bytes.len() as f64 / 1024.0);

    // 2. Upload to S3
    eprintln!("⬆ Uploading to s3://{staging_bucket}/{key}...");
    s3.put_object()
        .bucket(&staging_bucket)
        .key(&key)
        .body(tar_bytes.into())
        .send()
        .await
        .context("S3 PutObject failed")?;

    // 3. Generate presigned GET URL
    let presigned = s3
        .get_object()
        .bucket(&staging_bucket)
        .key(&key)
        .presigned(PresigningConfig::expires_in(expiry)?)
        .await?;
    let url = presigned.uri();

    // 4. ECS Exec: download and extract inside container
    let cmd = format!(
        "sh -c \"mkdir -p '{remote_path}' && (curl -sf '{url}' || wget -q -O - '{url}') | tar xzf - -C '{remote_path}'\""
    );
    eprintln!("⬇ Extracting to {remote_path} inside container...");

    let status = Command::new("aws")
        .args([
            "ecs", "execute-command",
            "--cluster", cluster,
            "--task", task,
            "--container", container,
            "--interactive",
            "--command", &cmd,
        ])
        .status()
        .context("failed to run aws ecs execute-command")?;

    if !status.success() {
        anyhow::bail!("ecs exec failed with status {}", status);
    }

    // 5. Cleanup
    s3.delete_object().bucket(&staging_bucket).key(&key).send().await?;
    eprintln!("✓ Synced {local_dir} → {cluster}/{task}/{container}:{remote_path}");
    Ok(())
}

/// Tar + gzip a directory into memory
fn tar_dir(path: &str) -> Result<Vec<u8>> {
    use std::io::Write;
    use std::process::Command;

    // Use system tar for simplicity (available on macOS/Linux)
    let output = Command::new("tar")
        .args(["czf", "-", "-C", path, "."])
        .output()
        .context("failed to run tar")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tar failed: {stderr}");
    }

    let _ = std::io::stdout().flush();
    Ok(output.stdout)
}

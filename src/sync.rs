use anyhow::{Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::Client as S3Client;
use std::process::Command;
use std::time::Duration;

use crate::cp::{ensure_bucket, get_staging_bucket};
use crate::exec;

/// Parse "cluster/task/container:/remote/path" into parts
pub fn parse_remote(s: &str) -> Result<(&str, &str, &str, &str)> {
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

/// Escape a string for safe use inside single-quoted shell arguments.
fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Sync a local directory to a remote container path.
pub async fn run(
    config: &aws_config::SdkConfig,
    local_dir: &str,
    remote: &str,
    bucket: Option<&str>,
    presign_expiry_secs: u64,
) -> Result<()> {
    let (cluster, task, container, remote_path) = parse_remote(remote)?;
    let s3 = S3Client::new(config);
    let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");
    let staging_bucket = get_staging_bucket(config, bucket).await?;
    ensure_bucket(&s3, &staging_bucket, region).await?;
    let key = format!("ecsctl/{}.tar.gz", uuid::Uuid::new_v4());
    let expiry = Duration::from_secs(presign_expiry_secs);

    // 1. Tar up local directory
    let tar_bytes = tar_dir(local_dir)?;

    // 2. Upload to S3
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
    let escaped_path = shell_escape(remote_path);
    let escaped_url = shell_escape(url);
    let cmd = format!(
        "sh -c 'mkdir -p '\"'\"'{}' \"'\"' && (curl -sf '\"'\"'{}' \"'\"' || wget -q -O - '\"'\"'{}' \"'\"') | tar xzf - -C '\"'\"'{}' \"'\"''",
        escaped_path, escaped_url, escaped_url, escaped_path
    );
    exec::non_interactive_exec(cluster, task, container, &cmd)
        .context("failed to extract archive inside container")?;

    // 5. Cleanup
    s3.delete_object()
        .bucket(&staging_bucket)
        .key(&key)
        .send()
        .await?;
    Ok(())
}

/// Sync a remote container path to a local directory.
pub async fn run_download(
    config: &aws_config::SdkConfig,
    remote: &str,
    local_dir: &str,
    bucket: Option<&str>,
    presign_expiry_secs: u64,
) -> Result<()> {
    let (cluster, task, container, remote_path) = parse_remote(remote)?;
    let s3 = S3Client::new(config);
    let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");
    let staging_bucket = get_staging_bucket(config, bucket).await?;
    ensure_bucket(&s3, &staging_bucket, region).await?;
    let key = format!("ecsctl/{}.tar.gz", uuid::Uuid::new_v4());
    let expiry = Duration::from_secs(presign_expiry_secs);

    // 1. Generate presigned PUT URL
    let presigned = s3
        .put_object()
        .bucket(&staging_bucket)
        .key(&key)
        .presigned(PresigningConfig::expires_in(expiry)?)
        .await?;
    let url = presigned.uri();

    // 2. ECS Exec: tar + upload to S3
    let escaped_path = shell_escape(remote_path);
    let escaped_url = shell_escape(url);
    let cmd = format!(
        "sh -c 'tar czf /tmp/_ecsctl_sync.tar.gz -C '\"'\"'{}' \"'\"' . && curl -sf -T /tmp/_ecsctl_sync.tar.gz '\"'\"'{}' \"'\"' && rm -f /tmp/_ecsctl_sync.tar.gz'",
        escaped_path, escaped_url
    );
    exec::non_interactive_exec(cluster, task, container, &cmd)
        .context("failed to compress and upload from container")?;

    // 3. Download from S3
    let resp = s3
        .get_object()
        .bucket(&staging_bucket)
        .key(&key)
        .send()
        .await
        .context("S3 GetObject failed")?;
    let bytes = resp.body.collect().await?.into_bytes();

    // 4. Extract locally
    std::fs::create_dir_all(local_dir)?;
    let mut child = Command::new("tar")
        .args(["xzf", "-", "-C", local_dir])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run tar")?;
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), &bytes)?;
    let tar_status = child.wait()?;
    if !tar_status.success() {
        anyhow::bail!("tar extract failed with status {}", tar_status);
    }

    // 5. Cleanup
    s3.delete_object()
        .bucket(&staging_bucket)
        .key(&key)
        .send()
        .await?;
    Ok(())
}

/// Tar + gzip a directory into memory
fn tar_dir(path: &str) -> Result<Vec<u8>> {
    let output = Command::new("tar")
        .args(["czf", "-", "-C", path, "."])
        .output()
        .context("failed to run tar")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tar failed: {stderr}");
    }

    Ok(output.stdout)
}

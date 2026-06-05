use anyhow::{bail, Context, Result};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_sts::Client as StsClient;
use std::process::Command as ProcessCommand;
use std::time::Duration;

/// Default presigned URL expiry. Must cover: ECS Exec API call (~2s) + SSM session
/// setup (~3-5s) + command start (~1-2s) + actual file transfer time.
pub const DEFAULT_PRESIGN_EXPIRY: Duration = Duration::from_secs(60);

/// Parse "cluster/task/container:/remote/path" into parts
fn parse_remote(s: &str) -> Option<(&str, &str, &str, &str)> {
    let colon_pos = s.find(':')?;
    let remote_path = &s[colon_pos + 1..];
    let prefix = &s[..colon_pos];
    let parts: Vec<&str> = prefix.splitn(3, '/').collect();
    if parts.len() == 3 {
        Some((parts[0], parts[1], parts[2], remote_path))
    } else {
        None
    }
}

fn is_remote(s: &str) -> bool {
    parse_remote(s).is_some()
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

async fn ensure_bucket(s3: &S3Client, bucket: &str, region: &str) -> Result<()> {
    match s3.head_bucket().bucket(bucket).send().await {
        Ok(_) => Ok(()),
        Err(_) => {
            let mut req = s3.create_bucket().bucket(bucket);
            if region != "us-east-1" {
                req = req.create_bucket_configuration(
                    aws_sdk_s3::types::CreateBucketConfiguration::builder()
                        .location_constraint(region.parse().unwrap())
                        .build(),
                );
            }
            req.send().await.context("failed to create staging bucket")?;
            eprintln!("✓ Created staging bucket: {bucket}");
            Ok(())
        }
    }
}

pub async fn run(
    config: &aws_config::SdkConfig,
    src: &str,
    dst: &str,
    bucket: Option<&str>,
    presign_expiry_secs: u64,
) -> Result<()> {
    let s3 = S3Client::new(config);
    let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");
    let staging_bucket = get_staging_bucket(config, bucket).await?;
    ensure_bucket(&s3, &staging_bucket, region).await?;

    let key = format!("ecsctl/{}.tar.gz", uuid::Uuid::new_v4());
    let expiry = Duration::from_secs(presign_expiry_secs);

    if !is_remote(src) && is_remote(dst) {
        upload(&s3, src, dst, &staging_bucket, &key, expiry).await
    } else if is_remote(src) && !is_remote(dst) {
        download(&s3, src, dst, &staging_bucket, &key, expiry).await
    } else {
        bail!("exactly one of src/dst must be a remote path (cluster/task/container:/path)")
    }
}

async fn upload(
    s3: &S3Client,
    local_path: &str,
    remote: &str,
    bucket: &str,
    key: &str,
    expiry: Duration,
) -> Result<()> {
    let (cluster, task, container, remote_path) =
        parse_remote(remote).context("invalid remote path")?;

    // 1. Upload local file to S3
    eprintln!("⬆ Uploading to s3://{bucket}/{key}...");
    let body = aws_sdk_s3::primitives::ByteStream::from_path(local_path)
        .await
        .context("failed to read local file")?;
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .send()
        .await
        .context("S3 PutObject failed")?;

    // 2. Generate presigned GET URL
    let presigned = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .presigned(PresigningConfig::expires_in(expiry)?)
        .await
        .context("failed to generate presigned URL")?;
    let url = presigned.uri();

    // 3. ECS Exec: download from presigned URL inside container
    let dest = if remote_path.is_empty() {
        let filename = std::path::Path::new(local_path).file_name().unwrap_or_default().to_string_lossy();
        format!("$HOME/{filename}")
    } else {
        remote_path.to_string()
    };
    let cmd = format!(
        "sh -c 'curl -sf -o \"{}\" \"{}\" || wget -q -O \"{}\" \"{}\"'",
        dest, url, dest, url
    );
    eprintln!("⬇ Downloading inside container to {dest}...");
    ecs_exec(cluster, task, container, &cmd)?;

    // 4. Cleanup S3
    s3.delete_object().bucket(bucket).key(key).send().await?;
    eprintln!("✓ Copied {local_path} → {cluster}/{task}/{container}:{dest}");
    Ok(())
}

async fn download(
    s3: &S3Client,
    remote: &str,
    local_path: &str,
    bucket: &str,
    key: &str,
    expiry: Duration,
) -> Result<()> {
    let (cluster, task, container, remote_path) =
        parse_remote(remote).context("invalid remote path")?;

    // 1. Generate presigned PUT URL
    let presigned = s3
        .put_object()
        .bucket(bucket)
        .key(key)
        .presigned(PresigningConfig::expires_in(expiry)?)
        .await
        .context("failed to generate presigned URL")?;
    let url = presigned.uri();

    // 2. ECS Exec: upload from container to S3 via presigned PUT
    let cmd = format!(
        "sh -c 'curl -sf -T \"{}\" \"{}\" || wget --method=PUT --body-file=\"{}\" \"{}\"'",
        remote_path, url, remote_path, url
    );
    eprintln!("⬆ Uploading from container {remote_path} to S3...");
    ecs_exec(cluster, task, container, &cmd)?;

    // 3. Download from S3 to local
    eprintln!("⬇ Downloading to {local_path}...");
    let resp = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .context("S3 GetObject failed")?;

    let bytes = resp.body.collect().await?.into_bytes();
    std::fs::write(local_path, &bytes)?;

    // 4. Cleanup
    s3.delete_object().bucket(bucket).key(key).send().await?;
    eprintln!("✓ Copied {cluster}/{task}/{container}:{remote_path} → {local_path}");
    Ok(())
}

/// Shell out to aws CLI for ECS Exec (only interactive mode is supported)
fn ecs_exec(cluster: &str, task: &str, container: &str, cmd: &str) -> Result<()> {
    let status = ProcessCommand::new("aws")
        .args([
            "ecs", "execute-command",
            "--cluster", cluster,
            "--task", task,
            "--container", container,
            "--interactive",
            "--command", cmd,
        ])
        .status()
        .context("failed to run aws ecs execute-command")?;

    if !status.success() {
        anyhow::bail!("ecs exec failed with status {}", status);
    }
    Ok(())
}

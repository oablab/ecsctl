use anyhow::{bail, Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_sts::Client as StsClient;
use std::time::Duration;

const PRESIGN_EXPIRY: Duration = Duration::from_secs(300);

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
) -> Result<()> {
    let s3 = S3Client::new(config);
    let ecs = EcsClient::new(config);
    let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");
    let staging_bucket = get_staging_bucket(config, bucket).await?;
    ensure_bucket(&s3, &staging_bucket, region).await?;

    let key = format!("ecsctl/{}.tar.gz", uuid::Uuid::new_v4());

    if !is_remote(src) && is_remote(dst) {
        // Upload: local -> container
        upload(config, &s3, &ecs, src, dst, &staging_bucket, &key).await
    } else if is_remote(src) && !is_remote(dst) {
        // Download: container -> local
        download(config, &s3, &ecs, src, dst, &staging_bucket, &key).await
    } else {
        bail!("exactly one of src/dst must be a remote path (cluster/task/container:/path)")
    }
}

async fn upload(
    _config: &aws_config::SdkConfig,
    s3: &S3Client,
    ecs: &EcsClient,
    local_path: &str,
    remote: &str,
    bucket: &str,
    key: &str,
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
        .presigned(PresigningConfig::expires_in(PRESIGN_EXPIRY)?)
        .await
        .context("failed to generate presigned URL")?;
    let url = presigned.uri();

    // 3. ECS Exec: download from presigned URL inside container
    let cmd = format!(
        "wget -q -O '{remote_path}' '{url}' 2>/dev/null || curl -sf -o '{remote_path}' '{url}'"
    );
    eprintln!("⬇ Downloading inside container to {remote_path}...");

    let resp = ecs
        .execute_command()
        .cluster(cluster)
        .task(task)
        .container(container)
        .interactive(false)
        .command(&cmd)
        .send()
        .await
        .context("ECS ExecuteCommand failed")?;

    if let Some(session) = resp.session() {
        eprintln!("  Session: {}", session.session_id().unwrap_or("unknown"));
    }

    // 4. Cleanup S3
    s3.delete_object().bucket(bucket).key(key).send().await?;
    eprintln!("✓ Copied {local_path} → {cluster}/{task}/{container}:{remote_path}");
    Ok(())
}

async fn download(
    _config: &aws_config::SdkConfig,
    s3: &S3Client,
    ecs: &EcsClient,
    remote: &str,
    local_path: &str,
    bucket: &str,
    key: &str,
) -> Result<()> {
    let (cluster, task, container, remote_path) =
        parse_remote(remote).context("invalid remote path")?;

    // 1. Generate presigned PUT URL
    let presigned = s3
        .put_object()
        .bucket(bucket)
        .key(key)
        .presigned(PresigningConfig::expires_in(PRESIGN_EXPIRY)?)
        .await
        .context("failed to generate presigned URL")?;
    let url = presigned.uri();

    // 2. ECS Exec: upload from container to S3 via presigned PUT
    let cmd = format!(
        "curl -sf -T '{remote_path}' '{url}' || wget --method=PUT --body-file='{remote_path}' '{url}'"
    );
    eprintln!("⬆ Uploading from container {remote_path} to S3...");

    ecs.execute_command()
        .cluster(cluster)
        .task(task)
        .container(container)
        .interactive(false)
        .command(&cmd)
        .send()
        .await
        .context("ECS ExecuteCommand failed")?;

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

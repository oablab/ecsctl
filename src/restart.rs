use anyhow::{Context, Result};
use aws_sdk_ecs::types::DeploymentConfiguration;
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    wait: bool,
    recreate: bool,
) -> Result<()> {
    let targets = cfg.resolve_targets(name);

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", name);
    }

    let ecs = EcsClient::new(config);

    if recreate {
        // Recreate is inherently serial per service: the deployment
        // configuration must be restored only after the deployment
        // stabilizes, otherwise ECS could apply the restored (rolling)
        // limits to the in-flight deployment.
        for alias in &targets {
            let (cluster, service) = resolve_alias(cfg, alias)?;
            recreate_service(&ecs, alias, &cluster, &service).await?;
        }
        return Ok(());
    }

    for alias in &targets {
        let (cluster, service) = resolve_alias(cfg, alias)?;

        eprintln!("🔄 Restarting {alias} ({service})...");
        ecs.update_service()
            .cluster(&cluster)
            .service(&service)
            .force_new_deployment(true)
            .send()
            .await
            .context(format!("UpdateService failed for {alias}"))?;

        eprintln!("✓ New deployment triggered for {alias}");
    }

    if wait && targets.len() == 1 {
        let alias = &targets[0];
        let (cluster, service) = resolve_alias(cfg, alias)?;
        eprintln!("⏳ Waiting for deployment to stabilize...");
        crate::apply::wait_for_stable(&ecs, &cluster, &service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

fn resolve_alias(cfg: &Config, alias: &str) -> Result<(String, String)> {
    let target = cfg
        .aliases
        .get(alias)
        .context(format!("alias '{alias}' not found"))?
        .clone();
    let parts: Vec<&str> = target.splitn(4, '/').collect();
    match parts.len() {
        2..=4 => Ok((parts[0].to_string(), parts[1].to_string())),
        _ => anyhow::bail!("invalid alias target for '{alias}'"),
    }
}

/// Stop-then-start restart: force `minimumHealthyPercent=0` /
/// `maximumPercent=100` so ECS must fully stop the old task (running its
/// shutdown hooks) before launching the replacement, then restore the
/// service's previous deployment configuration.
///
/// This avoids the rolling-update overlap window where two instances run
/// concurrently (duplicate bot tokens, OAuth refresh-token rotation races)
/// and where the new task seeds state from a backup taken before the old
/// task's final shutdown.
async fn recreate_service(
    ecs: &EcsClient,
    alias: &str,
    cluster: &str,
    service: &str,
) -> Result<()> {
    // Read the current deployment configuration so it can be restored.
    let desc = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context(format!("DescribeServices failed for {alias}"))?;
    let svc = desc.services().first().context(format!(
        "service '{service}' not found in cluster '{cluster}'"
    ))?;
    let saved = svc.deployment_configuration().cloned();
    // ECS rejects maximumPercent <= 100 while Availability Zone Rebalancing
    // is enabled — disable it for the recreate and restore it afterwards.
    let saved_az = svc.availability_zone_rebalancing().cloned();
    let az_enabled = matches!(
        saved_az,
        Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
    );

    // Preflight: if the container's stopTimeout is shorter than typical
    // shutdown hooks (e.g. state backup to S3), ECS will SIGKILL the task
    // mid-hook. Warn but proceed.
    if let Some(task_def_arn) = svc.task_definition() {
        if let Ok(td) = ecs
            .describe_task_definition()
            .task_definition(task_def_arn)
            .send()
            .await
        {
            let stop_timeout = td
                .task_definition()
                .and_then(|t| t.container_definitions().first())
                .and_then(|c| c.stop_timeout());
            match stop_timeout {
                Some(t) if t >= 120 => {}
                Some(t) => eprintln!(
                    "⚠️  {alias}: container stopTimeout is {t}s — shutdown hooks longer than this will be killed (recommend 120)"
                ),
                None => eprintln!(
                    "⚠️  {alias}: container stopTimeout not set (default 30s) — shutdown hooks longer than 30s will be killed (recommend 120)"
                ),
            }
        }
    }

    // Build the recreate configuration, preserving unrelated fields
    // (circuit breaker, alarms) from the saved configuration.
    let mut builder = DeploymentConfiguration::builder()
        .minimum_healthy_percent(0)
        .maximum_percent(100);
    if let Some(prev) = &saved {
        if let Some(cb) = prev.deployment_circuit_breaker() {
            builder = builder.deployment_circuit_breaker(cb.clone());
        }
        if let Some(alarms) = prev.alarms() {
            builder = builder.alarms(alarms.clone());
        }
    }

    eprintln!("🔄 Restarting {alias} ({service}) [recreate: stop old task first]...");
    let mut update = ecs
        .update_service()
        .cluster(cluster)
        .service(service)
        .force_new_deployment(true)
        .deployment_configuration(builder.build());
    if az_enabled {
        update = update.availability_zone_rebalancing(
            aws_sdk_ecs::types::AvailabilityZoneRebalancing::Disabled,
        );
    }
    update
        .send()
        .await
        .context(format!("UpdateService failed for {alias}"))?;

    // Must wait: restoring the rolling limits mid-deployment would let ECS
    // launch the replacement before the old task stops.
    eprintln!("⏳ Waiting for deployment to stabilize (old task stops before new one starts)...");
    crate::apply::wait_for_stable(ecs, cluster, service).await?;
    eprintln!("✓ Deployment stable for {alias}");

    // Restore the previous deployment configuration (and AZ rebalancing if
    // it was enabled). A config-only update does not launch or stop tasks.
    if saved.is_some() || az_enabled {
        let mut restore = ecs.update_service().cluster(cluster).service(service);
        if let Some(prev) = saved {
            restore = restore.deployment_configuration(prev);
        }
        if az_enabled {
            restore = restore.availability_zone_rebalancing(
                aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled,
            );
        }
        match restore.send().await {
            Ok(_) => eprintln!("✓ Restored previous deployment configuration for {alias}"),
            Err(e) => eprintln!(
                "⚠️  {alias}: failed to restore deployment configuration ({e}) — future deploys will also stop-first (min=0/max=100); restore manually with `aws ecs update-service`"
            ),
        }
    }

    Ok(())
}

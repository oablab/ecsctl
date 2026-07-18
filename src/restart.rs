use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_ecs::types::DeploymentConfiguration;
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

/// Upper bound on waiting for a recreate deployment to stabilize. If the
/// rollout is stuck, we restore the original deployment configuration and
/// fail instead of retaining the temporary stop-first policy indefinitely.
const RECREATE_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// Options for [`run_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RestartOptions {
    /// Wait for the deployment to stabilize before returning.
    pub wait: bool,
    /// Stop the old task first, then start the new one (implies waiting).
    pub recreate: bool,
}

/// Force a new rolling deployment (library-stable entry point).
pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    wait: bool,
) -> Result<()> {
    run_with(
        config,
        cfg,
        name,
        RestartOptions {
            wait,
            recreate: false,
        },
    )
    .await
}

/// Restart with explicit options. See [`RestartOptions`].
pub async fn run_with(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    opts: RestartOptions,
) -> Result<()> {
    let targets = cfg.resolve_targets(name);

    if targets.is_empty() {
        anyhow::bail!("group '{}' is empty or not found", name);
    }

    let ecs = EcsClient::new(config);

    if opts.recreate {
        // Recreate is inherently serial per service: the deployment
        // configuration must be restored only after the deployment
        // stabilizes, otherwise ECS could apply the restored (rolling)
        // limits to the in-flight deployment.
        for alias in &targets {
            let (cluster, service) = cfg.resolve_alias(alias)?;
            let (cluster, service) = (cluster.to_string(), service.to_string());
            recreate_service(&ecs, alias, &cluster, &service).await?;
        }
        return Ok(());
    }

    for alias in &targets {
        let (cluster, service) = cfg.resolve_alias(alias)?;

        eprintln!("🔄 Restarting {alias} ({service})...");
        ecs.update_service()
            .cluster(cluster)
            .service(service)
            .force_new_deployment(true)
            .send()
            .await
            .context(format!("UpdateService failed for {alias}"))?;

        eprintln!("✓ New deployment triggered for {alias}");
    }

    if opts.wait && targets.len() == 1 {
        let alias = &targets[0];
        let (cluster, service) = cfg.resolve_alias(alias)?;
        eprintln!("⏳ Waiting for deployment to stabilize...");
        crate::apply::wait_for_stable(&ecs, cluster, service).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
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
    // Read the current service state for preflight + restoration.
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

    // Preflight (fail closed): stop-then-start semantics are only guaranteed
    // for a singleton service on the default ECS rolling controller.
    // CODE_DEPLOY/EXTERNAL controllers ignore deploymentConfiguration
    // percentages, and with desired > 1 ECS interleaves per-task replacement
    // (max=100 caps the total, not "all old stopped before any new starts").
    if let Some(controller) = svc.deployment_controller().map(|c| c.r#type()) {
        if *controller != aws_sdk_ecs::types::DeploymentControllerType::Ecs {
            anyhow::bail!(
                "{alias}: --recreate requires the default ECS rolling deployment controller (found {controller:?}); CODE_DEPLOY/EXTERNAL controllers ignore deploymentConfiguration percentages"
            );
        }
    }
    let desired = svc.desired_count();
    if desired != 1 {
        anyhow::bail!(
            "{alias}: --recreate guarantees stop-then-start only for singleton services (desiredCount=1, found {desired}); with multiple tasks ECS interleaves per-task replacement"
        );
    }

    let saved = svc.deployment_configuration().cloned();
    // ECS rejects maximumPercent <= 100 while Availability Zone Rebalancing
    // is enabled — disable it for the recreate and restore it afterwards.
    let az_enabled = matches!(
        svc.availability_zone_rebalancing(),
        Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
    );

    // Preflight: if a container's stopTimeout is shorter than typical
    // shutdown hooks (e.g. state backup to S3), ECS will SIGKILL the task
    // mid-hook. Inspect every essential container; warn but proceed.
    match svc.task_definition() {
        Some(task_def_arn) => match ecs
            .describe_task_definition()
            .task_definition(task_def_arn)
            .send()
            .await
        {
            Ok(td) => {
                for c in td
                    .task_definition()
                    .map(|t| t.container_definitions())
                    .unwrap_or_default()
                    .iter()
                    .filter(|c| c.essential().unwrap_or(true))
                {
                    let cname = c.name().unwrap_or("<unnamed>");
                    match c.stop_timeout() {
                        Some(t) if t >= 120 => {}
                        Some(t) => eprintln!(
                            "⚠️  {alias}/{cname}: stopTimeout is {t}s — shutdown hooks longer than this will be killed (recommend 120)"
                        ),
                        None => eprintln!(
                            "⚠️  {alias}/{cname}: stopTimeout not set (default 30s) — shutdown hooks longer than 30s will be killed (recommend 120)"
                        ),
                    }
                }
            }
            Err(e) => eprintln!(
                "⚠️  {alias}: could not inspect task definition for stopTimeout preflight: {e}"
            ),
        },
        None => {
            eprintln!("⚠️  {alias}: service has no task definition; skipping stopTimeout preflight")
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
    let update_resp = update
        .send()
        .await
        .context(format!("UpdateService failed for {alias}"))?;

    // Identify the deployment this request created so success can be tied to
    // it (a circuit-breaker rollback creates a different deployment id).
    let expected_deployment = update_resp
        .service()
        .and_then(|s| {
            s.deployments()
                .iter()
                .find(|d| d.status().unwrap_or_default() == "PRIMARY")
        })
        .and_then(|d| d.id())
        .map(str::to_string);

    // From here on the temporary stop-first policy is live: whatever happens
    // while waiting, always attempt restoration before returning.
    eprintln!("⏳ Waiting for deployment to stabilize (old task stops before new one starts)...");
    let wait_result: Result<()> = match tokio::time::timeout(
        RECREATE_WAIT_TIMEOUT,
        crate::apply::wait_for_stable(ecs, cluster, service),
    )
    .await
    {
        Ok(Ok(())) => {
            verify_deployment(ecs, cluster, service, expected_deployment.as_deref()).await
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!(
            "timed out after {}s waiting for the recreate deployment to stabilize",
            RECREATE_WAIT_TIMEOUT.as_secs()
        )),
    };
    if wait_result.is_ok() {
        eprintln!("✓ Deployment stable for {alias}");
    }

    // Restore the previous deployment configuration (and AZ rebalancing if
    // it was enabled). A config-only update does not launch or stop tasks.
    let restore_result = restore_with_retry(ecs, cluster, service, saved, az_enabled).await;
    match &restore_result {
        Ok(()) => eprintln!("✓ Restored previous deployment configuration for {alias}"),
        Err(e) => eprintln!(
            "❌ {alias}: failed to restore deployment configuration ({e}) — the service is left with min=0/max=100 (future deploys stop-first); restore manually with `aws ecs update-service`"
        ),
    }

    // Propagate failures: a wait failure takes precedence (the deployment
    // itself is suspect); a restore failure alone must still fail the command
    // so automation does not report success with the temporary policy active.
    match (wait_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(we), Ok(())) => Err(we.context(format!(
            "{alias}: recreate deployment did not stabilize (original deployment configuration was restored)"
        ))),
        (Ok(()), Err(re)) => Err(re.context(format!(
            "{alias}: recreate deployment succeeded but restoring the deployment configuration failed"
        ))),
        (Err(we), Err(_)) => Err(we.context(format!(
            "{alias}: recreate deployment did not stabilize AND the deployment configuration could not be restored"
        ))),
    }
}

/// Confirm the service's primary deployment is the one this recreate created
/// and that it completed — a rollback or a concurrent deploy would surface a
/// different primary deployment id.
async fn verify_deployment(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    expected_id: Option<&str>,
) -> Result<()> {
    let Some(expected_id) = expected_id else {
        // UpdateService did not return a deployment id; nothing to verify.
        return Ok(());
    };
    let desc = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed during post-wait verification")?;
    let svc = desc.services().first().context("service not found")?;
    let primary = svc
        .deployments()
        .iter()
        .find(|d| d.status().unwrap_or_default() == "PRIMARY")
        .context("no PRIMARY deployment found")?;
    let actual = primary.id().unwrap_or_default();
    if actual != expected_id {
        anyhow::bail!(
            "service stabilized on deployment {actual}, not the recreate deployment {expected_id} — it was likely rolled back or superseded by a concurrent deploy"
        );
    }
    Ok(())
}

/// Restore the saved deployment configuration and AZ-rebalancing setting,
/// retrying transient failures before giving up.
async fn restore_with_retry(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    saved: Option<DeploymentConfiguration>,
    az_enabled: bool,
) -> Result<()> {
    if saved.is_none() && !az_enabled {
        return Ok(());
    }
    const ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        let mut restore = ecs.update_service().cluster(cluster).service(service);
        if let Some(prev) = saved.clone() {
            restore = restore.deployment_configuration(prev);
        }
        if az_enabled {
            restore = restore.availability_zone_rebalancing(
                aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled,
            );
        }
        match restore.send().await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if attempt < ATTEMPTS {
                    eprintln!("⚠️  restore attempt {attempt}/{ATTEMPTS} failed, retrying: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::anyhow!(
        "all {ATTEMPTS} restore attempts failed: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

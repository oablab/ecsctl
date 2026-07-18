use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_ecs::types::DeploymentConfiguration;
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

/// Upper bound on waiting for a recreate deployment to stabilize. If the
/// rollout is stuck, we restore the original deployment configuration and
/// fail instead of retaining the temporary stop-first policy indefinitely.
const RECREATE_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// Copy the complete deployment configuration while overriding only the two
/// percentages that implement stop-first replacement. Cloning (rather than
/// enumerating fields on a builder) keeps fields added by future SDK
/// versions intact — `DeploymentConfiguration` is `#[non_exhaustive]` with
/// public fields.
fn recreate_deployment_configuration(saved: &DeploymentConfiguration) -> DeploymentConfiguration {
    let mut cfg = saved.clone();
    cfg.minimum_healthy_percent = Some(0);
    cfg.maximum_percent = Some(100);
    cfg
}

/// Merge base for restoration: take the service's *current* configuration
/// (preserving any legitimate concurrent changes to fields this operation
/// does not own, e.g. alarms or circuit-breaker edits by IaC) and restore
/// only the two percentages this operation changed.
fn merged_restore_configuration(
    current: &DeploymentConfiguration,
    saved: &DeploymentConfiguration,
) -> DeploymentConfiguration {
    let mut cfg = current.clone();
    cfg.minimum_healthy_percent = saved.minimum_healthy_percent;
    cfg.maximum_percent = saved.maximum_percent;
    cfg
}

/// Whether the service still carries this invocation's temporary values for
/// the fields it owns (min/max percentages, and the AZ-rebalancing flag when
/// this invocation disabled it). If not, another writer took ownership and
/// restoration must fail closed instead of overwriting their change.
fn temp_policy_owned(
    current: &DeploymentConfiguration,
    current_az_enabled: bool,
    az_was_enabled: bool,
) -> bool {
    current.minimum_healthy_percent() == Some(0)
        && current.maximum_percent() == Some(100)
        && (!az_was_enabled || !current_az_enabled)
}

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

/// Cancellation signal for [`run_with_cancel`]. The sender side is owned by
/// the caller (typically the CLI binary registering Ctrl-C/SIGTERM); sending
/// `true` requests graceful cancellation — an in-flight recreate restores the
/// service's deployment configuration before returning an error.
pub type CancelSignal = tokio::sync::watch::Receiver<bool>;

/// Create a cancellation channel for [`run_with_cancel`].
pub fn cancel_channel() -> (tokio::sync::watch::Sender<bool>, CancelSignal) {
    tokio::sync::watch::channel(false)
}

/// Resolve when cancellation is requested. If the sender is dropped without
/// ever signalling, this pends forever (no cancellation).
async fn cancelled(rx: &mut CancelSignal) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Restart with explicit options. See [`RestartOptions`].
///
/// This entry point performs no signal handling of its own; cancellation is
/// not available. CLI callers that want Ctrl-C/SIGTERM to trigger graceful
/// restoration should use [`run_with_cancel`] and own signal registration at
/// the binary boundary.
pub async fn run_with(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    opts: RestartOptions,
) -> Result<()> {
    let (_tx, rx) = cancel_channel();
    run_with_cancel(config, cfg, name, opts, rx).await
}

/// Restart with explicit options and a caller-owned cancellation signal.
pub async fn run_with_cancel(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    opts: RestartOptions,
    cancel: CancelSignal,
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
            let mut cancel = cancel.clone();
            recreate_service(&ecs, alias, &cluster, &service, &mut cancel).await?;
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
    cancel: &mut CancelSignal,
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

    // Preflight (fail closed) + snapshot capture. See `preflight` for the
    // guarantees being enforced.
    let (saved, az_enabled) = preflight(alias, svc)?;

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

    // Preserve every deploymentConfiguration field and change only min/max.
    let recreate_config = recreate_deployment_configuration(&saved);

    // Print the recovery snapshot BEFORE mutating: if this process is killed
    // (SIGKILL, crash) after the update, no in-process cleanup can run — the
    // operator recovers from this line. Ctrl-C is handled below.
    eprintln!(
        "ℹ️  {alias}: recovery snapshot — minimumHealthyPercent={} maximumPercent={} availabilityZoneRebalancing={}",
        saved.minimum_healthy_percent().unwrap_or(100),
        saved.maximum_percent().unwrap_or(200),
        if az_enabled { "ENABLED" } else { "DISABLED" },
    );

    eprintln!("🔄 Restarting {alias} ({service}) [recreate: stop old task first]...");
    let mut update = ecs
        .update_service()
        .cluster(cluster)
        .service(service)
        .force_new_deployment(true)
        .deployment_configuration(recreate_config);
    if az_enabled {
        update = update.availability_zone_rebalancing(
            aws_sdk_ecs::types::AvailabilityZoneRebalancing::Disabled,
        );
    }
    let update_resp = match update.send().await {
        Ok(resp) => resp,
        Err(e) => {
            // Ambiguous-write reconciliation: a transport error or timeout
            // does not prove the write was rejected — AWS may have committed
            // the temporary policy. Read the service back; if the stop-first
            // values are live, restore before returning the original error.
            eprintln!("⚠️  {alias}: UpdateService failed; reconciling whether the temporary policy was applied...");
            match ecs
                .describe_services()
                .cluster(cluster)
                .services(service)
                .send()
                .await
            {
                Ok(desc) => {
                    let live = desc.services().first().and_then(|s| {
                        s.deployment_configuration().map(|c| {
                            let az_now = matches!(
                                s.availability_zone_rebalancing(),
                                Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
                            );
                            temp_policy_owned(c, az_now, az_enabled)
                                || (az_enabled && !az_now)
                                || c.minimum_healthy_percent() == Some(0)
                                || c.maximum_percent() == Some(100)
                        })
                    });
                    if live == Some(true) {
                        eprintln!("⚠️  {alias}: temporary policy is live despite the error — restoring...");
                        match restore_with_retry(ecs, cluster, service, &saved, az_enabled).await {
                            Ok(()) => eprintln!("✓ Restored previous deployment configuration for {alias}"),
                            Err(re) => eprintln!(
                                "❌ {alias}: restoration after ambiguous write also failed ({re}) — restore manually using the recovery snapshot above"
                            ),
                        }
                    }
                }
                Err(de) => eprintln!(
                    "❌ {alias}: could not reconcile ambiguous write ({de}) — verify the service manually against the recovery snapshot above"
                ),
            }
            return Err(anyhow::anyhow!(e).context(format!("UpdateService failed for {alias}")));
        }
    };

    // Identify the deployment this request created so success can be tied to
    // it (a circuit-breaker rollback creates a different deployment id). A
    // missing id disables the identity check, so it is a verification error
    // (fail closed) rather than a silent pass.
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
    // while waiting — including caller-signalled cancellation (Ctrl-C or
    // SIGTERM registered at the binary boundary) — always attempt
    // restoration before returning. (A hard kill cannot be intercepted; see
    // the recovery snapshot printed above.)
    eprintln!("⏳ Waiting for deployment to stabilize (old task stops before new one starts)...");
    let wait_result: Result<()> = tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(anyhow::anyhow!(
            "cancelled while waiting for the recreate deployment"
        )),
        res = tokio::time::timeout(
            RECREATE_WAIT_TIMEOUT,
            crate::apply::wait_for_stable(ecs, cluster, service),
        ) => match res {
            Ok(Ok(())) => {
                verify_deployment(ecs, cluster, service, expected_deployment.as_deref()).await
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {}s waiting for the recreate deployment to stabilize",
                RECREATE_WAIT_TIMEOUT.as_secs()
            )),
        },
    };
    if wait_result.is_ok() {
        eprintln!("✓ Deployment stable for {alias}");
    }

    // Restore the previous deployment configuration (and AZ rebalancing if
    // it was enabled). A config-only update does not launch or stop tasks.
    let restore_result = restore_with_retry(ecs, cluster, service, &saved, az_enabled).await;
    match &restore_result {
        Ok(()) => eprintln!("✓ Restored previous deployment configuration for {alias}"),
        Err(e) => eprintln!(
            "❌ {alias}: failed to restore deployment configuration ({e}) — the service is left with min=0/max=100 (future deploys stop-first); restore manually with `aws ecs update-service` using the recovery snapshot above"
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
    // Fail closed: without the id of the deployment this recreate created,
    // the identity check below cannot run, and a rolled-back or concurrently
    // superseded deployment could be mistaken for success.
    let expected_id = expected_id.context(
        "UpdateService returned no PRIMARY deployment id — cannot verify the recreate deployment",
    )?;
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
/// retrying transient failures, and read the service back to verify the
/// restored fields actually match the snapshot (an HTTP success alone does
/// not prove convergence, e.g. under concurrent IaC/operator updates).
/// Restore the fields this operation changed (min/max percentages, AZ
/// rebalancing), retrying transient failures.
///
/// Each attempt is ownership-aware: it reads the current service first and
/// verifies the operation-owned fields still carry this invocation's
/// temporary values. If another writer (IaC, operator) changed them, we no
/// longer own the state and fail closed instead of overwriting their change.
/// The restore payload is a merge — the *current* configuration with only
/// min/max reset from the snapshot — so legitimate concurrent changes to
/// unowned fields (alarms, circuit breaker, hooks) are preserved.
///
/// Residual limitation: ECS offers no compare-and-swap token, so a writer
/// racing between our read and write (TOCTOU window) can still be
/// overwritten on the owned fields; unowned fields are never overwritten
/// beyond that window.
async fn restore_with_retry(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    saved: &DeploymentConfiguration,
    az_enabled: bool,
) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        let result = restore_once(ecs, cluster, service, saved, az_enabled).await;
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Ownership loss is terminal — retrying would overwrite the
                // concurrent writer's change.
                if e.to_string().contains("ownership lost") {
                    return Err(e);
                }
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

/// One ownership-checked restore attempt: read, verify ownership, merge,
/// write, read back and verify the owned fields converged.
async fn restore_once(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    saved: &DeploymentConfiguration,
    az_enabled: bool,
) -> Result<()> {
    // Pre-write read: confirm we still own the temporary state.
    let desc = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed before restoration")?;
    let svc = desc.services().first().context("service not found")?;
    let current = svc
        .deployment_configuration()
        .context("service has no deploymentConfiguration before restore")?;
    let current_az_enabled = matches!(
        svc.availability_zone_rebalancing(),
        Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
    );

    // Already restored (e.g. by a previous attempt whose response was lost)?
    if current.minimum_healthy_percent() == saved.minimum_healthy_percent()
        && current.maximum_percent() == saved.maximum_percent()
        && current_az_enabled == az_enabled
    {
        return Ok(());
    }

    if !temp_policy_owned(current, current_az_enabled, az_enabled) {
        anyhow::bail!(
            "ownership lost: the service's deployment configuration was changed concurrently (min/max are {:?}/{:?}) — not overwriting; reconcile manually or via your IaC",
            current.minimum_healthy_percent(),
            current.maximum_percent(),
        );
    }

    // Merge: current configuration with only the owned fields restored.
    let merged = merged_restore_configuration(current, saved);
    let mut restore = ecs
        .update_service()
        .cluster(cluster)
        .service(service)
        .deployment_configuration(merged);
    if az_enabled {
        restore = restore.availability_zone_rebalancing(
            aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled,
        );
    }
    restore
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    verify_restored(ecs, cluster, service, saved, az_enabled).await
}

/// Compare the operation-owned fields (min/max percentages) after a restore.
/// Unowned fields are intentionally not compared: the merge-restore preserves
/// concurrent changes to them, so they may legitimately differ from the
/// pre-mutation snapshot.
fn validate_restored_configuration(
    current: &DeploymentConfiguration,
    saved: &DeploymentConfiguration,
) -> Result<()> {
    if current.minimum_healthy_percent() == saved.minimum_healthy_percent()
        && current.maximum_percent() == saved.maximum_percent()
    {
        return Ok(());
    }
    anyhow::bail!(
        "restored min/max are {:?}/{:?}, expected {:?}/{:?} (concurrent update?)",
        current.minimum_healthy_percent(),
        current.maximum_percent(),
        saved.minimum_healthy_percent(),
        saved.maximum_percent(),
    );
}

/// Read the service back and compare the operation-owned fields (min/max
/// percentages and the AZ-rebalancing flag) against the snapshot.
async fn verify_restored(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
    saved: &DeploymentConfiguration,
    az_enabled: bool,
) -> Result<()> {
    let desc = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed during restore verification")?;
    let svc = desc.services().first().context("service not found")?;
    let current = svc
        .deployment_configuration()
        .context("service has no deploymentConfiguration after restore")?;

    validate_restored_configuration(current, saved)?;
    let current_az_enabled = matches!(
        svc.availability_zone_rebalancing(),
        Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
    );
    if current_az_enabled != az_enabled {
        anyhow::bail!(
            "availabilityZoneRebalancing is {} after restore, expected {}",
            if current_az_enabled {
                "ENABLED"
            } else {
                "DISABLED"
            },
            if az_enabled { "ENABLED" } else { "DISABLED" },
        );
    }
    Ok(())
}

/// Fail-closed preflight for `--recreate`, returning the restoration
/// snapshot `(deployment_configuration, az_rebalancing_was_enabled)`.
///
/// Stop-then-start semantics are only guaranteed for a singleton REPLICA
/// service on the default ECS rolling controller with the ROLLING deployment
/// strategy. CODE_DEPLOY/EXTERNAL controllers and native
/// BLUE_GREEN/LINEAR/CANARY strategies ignore or reinterpret
/// deploymentConfiguration percentages; DAEMON scheduling has no
/// desiredCount semantics; and with desired > 1 ECS interleaves per-task
/// replacement (max=100 caps the total, not "all old stopped before any new
/// starts"). A missing deploymentConfiguration aborts before mutation since
/// restoration could not be guaranteed.
fn preflight(
    alias: &str,
    svc: &aws_sdk_ecs::types::Service,
) -> Result<(DeploymentConfiguration, bool)> {
    if let Some(controller) = svc.deployment_controller().map(|c| c.r#type()) {
        if *controller != aws_sdk_ecs::types::DeploymentControllerType::Ecs {
            anyhow::bail!(
                "{alias}: --recreate requires the default ECS rolling deployment controller (found {controller:?}); CODE_DEPLOY/EXTERNAL controllers ignore deploymentConfiguration percentages"
            );
        }
    }
    if let Some(sched) = svc.scheduling_strategy() {
        if *sched != aws_sdk_ecs::types::SchedulingStrategy::Replica {
            anyhow::bail!(
                "{alias}: --recreate requires REPLICA scheduling (found {sched:?}); DAEMON services have no singleton stop-then-start semantics"
            );
        }
    }
    let desired = svc.desired_count();
    if desired != 1 {
        anyhow::bail!(
            "{alias}: --recreate guarantees stop-then-start only for singleton services (desiredCount=1, found {desired}); with multiple tasks ECS interleaves per-task replacement"
        );
    }
    let saved = svc.deployment_configuration().cloned().with_context(|| {
        format!(
            "{alias}: DescribeServices returned no deploymentConfiguration — cannot guarantee restoration, aborting before mutation"
        )
    })?;
    if let Some(strategy) = saved.strategy() {
        if *strategy != aws_sdk_ecs::types::DeploymentStrategy::Rolling {
            anyhow::bail!(
                "{alias}: --recreate requires the ROLLING deployment strategy (found {strategy:?}); min/max percentages do not provide stop-then-start under native blue/green or canary strategies"
            );
        }
    }
    let az_enabled = matches!(
        svc.availability_zone_rebalancing(),
        Some(aws_sdk_ecs::types::AvailabilityZoneRebalancing::Enabled)
    );
    Ok((saved, az_enabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ecs::types::{
        AvailabilityZoneRebalancing, CanaryConfiguration, DeploymentController,
        DeploymentControllerType, DeploymentLifecycleHook, DeploymentStrategy, LinearConfiguration,
        SchedulingStrategy, Service,
    };

    fn saved_configuration() -> DeploymentConfiguration {
        DeploymentConfiguration::builder()
            .minimum_healthy_percent(75)
            .maximum_percent(150)
            .strategy(DeploymentStrategy::Rolling)
            .bake_time_in_minutes(7)
            .lifecycle_hooks(DeploymentLifecycleHook::builder().build())
            .linear_configuration(
                LinearConfiguration::builder()
                    .step_percent(10.0)
                    .step_bake_time_in_minutes(3)
                    .build(),
            )
            .canary_configuration(
                CanaryConfiguration::builder()
                    .canary_percent(5.0)
                    .canary_bake_time_in_minutes(4)
                    .build(),
            )
            .build()
    }

    fn base_service() -> Service {
        Service::builder()
            .service_name("svc")
            .desired_count(1)
            .deployment_controller(
                DeploymentController::builder()
                    .r#type(DeploymentControllerType::Ecs)
                    .build()
                    .unwrap(),
            )
            .scheduling_strategy(SchedulingStrategy::Replica)
            .deployment_configuration(
                DeploymentConfiguration::builder()
                    .minimum_healthy_percent(100)
                    .maximum_percent(200)
                    .build(),
            )
            .build()
    }

    #[test]
    fn preflight_accepts_singleton_replica_rolling() {
        let (saved, az) = preflight("t", &base_service()).expect("should pass");
        assert_eq!(saved.minimum_healthy_percent(), Some(100));
        assert_eq!(saved.maximum_percent(), Some(200));
        assert!(!az, "AZ rebalancing unset means disabled");
    }

    #[test]
    fn preflight_reports_az_rebalancing_enabled() {
        let svc = Service::builder()
            .set_deployment_controller(base_service().deployment_controller().cloned())
            .set_scheduling_strategy(base_service().scheduling_strategy().cloned())
            .set_deployment_configuration(base_service().deployment_configuration().cloned())
            .desired_count(1)
            .availability_zone_rebalancing(AvailabilityZoneRebalancing::Enabled)
            .build();
        let (_, az) = preflight("t", &svc).expect("should pass");
        assert!(az);
    }

    #[test]
    fn preflight_rejects_non_ecs_controller() {
        let svc = Service::builder()
            .desired_count(1)
            .deployment_controller(
                DeploymentController::builder()
                    .r#type(DeploymentControllerType::CodeDeploy)
                    .build()
                    .unwrap(),
            )
            .build();
        let err = preflight("t", &svc).unwrap_err().to_string();
        assert!(err.contains("deployment controller"), "got: {err}");
    }

    #[test]
    fn preflight_rejects_daemon_scheduling() {
        let svc = Service::builder()
            .desired_count(1)
            .scheduling_strategy(SchedulingStrategy::Daemon)
            .build();
        let err = preflight("t", &svc).unwrap_err().to_string();
        assert!(err.contains("REPLICA"), "got: {err}");
    }

    #[test]
    fn preflight_rejects_non_singleton() {
        for desired in [0, 2] {
            let svc = Service::builder().desired_count(desired).build();
            let err = preflight("t", &svc).unwrap_err().to_string();
            assert!(err.contains("desiredCount=1"), "got: {err}");
        }
    }

    #[test]
    fn preflight_rejects_missing_snapshot() {
        // desired=1 and default controller/scheduler, but no
        // deploymentConfiguration → must abort before mutation.
        let svc = Service::builder().desired_count(1).build();
        let err = preflight("t", &svc).unwrap_err().to_string();
        assert!(err.contains("cannot guarantee restoration"), "got: {err}");
    }

    #[test]
    fn preflight_rejects_blue_green_strategy() {
        let svc = Service::builder()
            .desired_count(1)
            .deployment_configuration(
                DeploymentConfiguration::builder()
                    .minimum_healthy_percent(100)
                    .maximum_percent(200)
                    .strategy(DeploymentStrategy::BlueGreen)
                    .build(),
            )
            .build();
        let err = preflight("t", &svc).unwrap_err().to_string();
        assert!(err.contains("ROLLING"), "got: {err}");
    }

    #[test]
    fn recreate_configuration_changes_only_percentages() {
        let saved = saved_configuration();
        let recreate = recreate_deployment_configuration(&saved);

        assert_eq!(recreate.minimum_healthy_percent(), Some(0));
        assert_eq!(recreate.maximum_percent(), Some(100));
        assert_eq!(
            recreate.deployment_circuit_breaker(),
            saved.deployment_circuit_breaker()
        );
        assert_eq!(recreate.alarms(), saved.alarms());
        assert_eq!(recreate.strategy(), saved.strategy());
        assert_eq!(
            recreate.bake_time_in_minutes(),
            saved.bake_time_in_minutes()
        );
        assert_eq!(recreate.lifecycle_hooks(), saved.lifecycle_hooks());
        assert_eq!(
            recreate.linear_configuration(),
            saved.linear_configuration()
        );
        assert_eq!(
            recreate.canary_configuration(),
            saved.canary_configuration()
        );
    }

    #[test]
    fn restore_validation_compares_owned_fields_only() {
        let saved = saved_configuration();
        // min/max diverge → error
        let mut wrong = saved.clone();
        wrong.minimum_healthy_percent = Some(0);
        wrong.maximum_percent = Some(100);
        let error = validate_restored_configuration(&wrong, &saved).unwrap_err();
        assert!(error.to_string().contains("expected"), "got: {error}");
        // unowned field diverges (concurrent change preserved by merge) → ok
        let mut concurrent = saved.clone();
        concurrent.bake_time_in_minutes = Some(42);
        assert!(validate_restored_configuration(&concurrent, &saved).is_ok());
        assert!(validate_restored_configuration(&saved, &saved).is_ok());
    }

    #[test]
    fn merged_restore_preserves_concurrent_unowned_changes() {
        let saved = saved_configuration();
        // Simulate: while we held the temp policy, IaC changed bake time.
        let mut current = recreate_deployment_configuration(&saved);
        current.bake_time_in_minutes = Some(42);

        let merged = merged_restore_configuration(&current, &saved);
        // Owned fields restored from the snapshot…
        assert_eq!(
            merged.minimum_healthy_percent(),
            saved.minimum_healthy_percent()
        );
        assert_eq!(merged.maximum_percent(), saved.maximum_percent());
        // …unowned concurrent change preserved, not rolled back.
        assert_eq!(merged.bake_time_in_minutes(), Some(42));
    }

    #[test]
    fn ownership_detection() {
        let saved = saved_configuration();
        let temp = recreate_deployment_configuration(&saved);
        // Our temp values, AZ disabled after we disabled it → owned.
        assert!(temp_policy_owned(&temp, false, true));
        // AZ re-enabled by someone else while we had disabled it → lost.
        assert!(!temp_policy_owned(&temp, true, true));
        // AZ was never ours to manage → AZ state irrelevant.
        assert!(temp_policy_owned(&temp, true, false));
        // Percentages changed by a concurrent writer → lost.
        let mut foreign = temp.clone();
        foreign.maximum_percent = Some(200);
        assert!(!temp_policy_owned(&foreign, false, true));
    }
}

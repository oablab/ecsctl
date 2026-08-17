use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use std::collections::HashMap;

use crate::config::Config;

/// Whether a container definition uses any field the flat single-container
/// export form cannot represent, and must therefore export through the
/// containers[] form instead. Pulled out as its own function so the decision
/// is testable without an AWS client -- ContainerDefinition::builder() is a
/// pure local constructor.
fn container_uses_new_fields(cd: &aws_sdk_ecs::types::ContainerDefinition) -> bool {
    cd.user().is_some()
        || cd.readonly_root_filesystem().unwrap_or(false)
        || !cd.entry_point().is_empty()
        || !cd.depends_on().is_empty()
        || cd.linux_parameters().is_some()
        || !cd.mount_points().is_empty()
}

/// Convert one ECS container definition into a `ContainerSpec`.
///
/// One function rather than three hand-copied blocks. The previous shape had
/// this mapping duplicated across the multi-container loop, the
/// single-container-with-new-fields branch, and the flat branch, which is the
/// drift that produced this PR's original export bug (new fields added to one
/// branch only) and contributed to the `containerName` round-trip break. Any
/// future field is now added once, and every export path shares the same
/// lossless-or-refuse policy.
fn container_to_spec(
    cd: &aws_sdk_ecs::types::ContainerDefinition,
) -> Result<crate::apply::ContainerSpec> {
    let name = cd.name().unwrap_or("app").to_string();

    let mut env: HashMap<String, String> = HashMap::new();
    for kv in cd.environment() {
        if let (Some(k), Some(v)) = (kv.name(), kv.value()) {
            env.insert(k.to_string(), v.to_string());
        }
    }

    let mut secrets: HashMap<String, String> = HashMap::new();
    for sec in cd.secrets() {
        secrets.insert(sec.name().to_string(), sec.value_from().to_string());
    }

    let log_group = cd
        .log_configuration()
        .and_then(|lc| lc.options())
        .and_then(|opts| opts.get("awslogs-group"))
        .map(|s| s.to_string());

    let port = cd
        .port_mappings()
        .first()
        .map(|p| p.container_port().unwrap_or(0) as u16)
        .unwrap_or(0);

    let vec_or_none = |v: &[String]| -> Option<Vec<String>> {
        if v.is_empty() {
            None
        } else {
            Some(v.to_vec())
        }
    };

    let mut depends_on: Vec<crate::apply::DependsOn> = Vec::new();
    for d in cd.depends_on() {
        // An externally created task definition can legally use HEALTHY, which
        // this spec parses but refuses on apply. Carrying it through would emit
        // a spec that cannot be reapplied, so it is dropped -- but loudly:
        // dropping a startup-ordering constraint in silence is how an app
        // container ends up running before its dependency is ready.
        let condition = match d.condition() {
            aws_sdk_ecs::types::ContainerCondition::Start => {
                crate::apply::DependsOnCondition::Start
            }
            aws_sdk_ecs::types::ContainerCondition::Success => {
                crate::apply::DependsOnCondition::Success
            }
            aws_sdk_ecs::types::ContainerCondition::Complete => {
                crate::apply::DependsOnCondition::Complete
            }
            other => {
                eprintln!(
                    "  \u{26a0}\u{fe0f}  dropping dependsOn {} -> {} (condition {}): not representable in this spec, so the exported YAML does not preserve that startup ordering",
                    name,
                    d.container_name(),
                    other.as_str()
                );
                continue;
            }
        };
        depends_on.push(crate::apply::DependsOn {
            container_name: d.container_name().to_string(),
            condition,
        });
    }

    let linux_parameters = match cd.linux_parameters().and_then(|lp| lp.capabilities()) {
        None => None,
        Some(c) => {
            if !c.add().is_empty() {
                anyhow::bail!(
                    "container '{}' adds Linux capabilities ({}), which this spec cannot represent. Export refused rather than silently dropping them",
                    name,
                    c.add().join(", ")
                );
            }
            // apply's validate() hard-rejects anything outside its known
            // capability list, so exporting a name it does not know would
            // produce a spec that fails on reapply with a "typo" error naming a
            // value the user never typed.
            for cap in c.drop() {
                if !crate::apply::is_known_capability(cap) {
                    anyhow::bail!(
                        "container '{}' drops Linux capability '{}', which this spec's validation does not recognise. Export refused rather than emitting a spec that cannot be reapplied",
                        name,
                        cap
                    );
                }
            }
            Some(crate::apply::LinuxParameters {
                capabilities_drop: c.drop().iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let mut mount_points: std::collections::BTreeMap<String, crate::apply::MountPointSpec> =
        std::collections::BTreeMap::new();
    for mp in cd.mount_points() {
        if let (Some(cp), Some(sv)) = (mp.container_path(), mp.source_volume()) {
            // Preserve readOnly. Dropping it silently turned a read-only mount
            // into a writable one on reapply.
            let spec = if mp.read_only().unwrap_or(false) {
                crate::apply::MountPointSpec::Detailed(crate::apply::DetailedMountPoint {
                    source_volume: sv.to_string(),
                    read_only: true,
                })
            } else {
                crate::apply::MountPointSpec::Volume(sv.to_string())
            };
            mount_points.insert(cp.to_string(), spec);
        }
    }

    Ok(crate::apply::ContainerSpec {
        name,
        image: cd.image().unwrap_or("?").to_string(),
        essential: cd.essential().unwrap_or(true),
        port,
        command: vec_or_none(cd.command()),
        entry_point: vec_or_none(cd.entry_point()),
        env,
        secrets,
        log_group,
        user: cd.user().map(|s| s.to_string()),
        readonly_root_filesystem: cd.readonly_root_filesystem().unwrap_or(false),
        depends_on,
        linux_parameters,
        mount_points,
    })
}

pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    output: Option<&str>,
    json: bool,
) -> Result<()> {
    let target = cfg
        .aliases
        .get(name)
        .context(format!("alias '{name}' not found"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2..=4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target"),
    };

    let ecs = EcsClient::new(config);
    let spec = build_spec(&ecs, cluster, service).await?;

    let out = if json {
        serde_json::to_string_pretty(&spec).context("failed to serialize JSON")?
    } else {
        serde_yaml::to_string(&spec).context("failed to serialize YAML")?
    };

    match output {
        Some(out_file) => {
            std::fs::write(out_file, &out)?;
            eprintln!("✓ Exported {cluster}/{service} → {out_file}");
        }
        None => {
            print!("{out}");
        }
    }
    Ok(())
}

async fn build_spec(
    ecs: &EcsClient,
    cluster: &str,
    service: &str,
) -> Result<crate::apply::ServiceSpec> {
    // Get service details
    let svc_resp = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed")?;

    let svc = svc_resp.services().first().context("service not found")?;

    let task_def_arn = svc.task_definition().context("no task definition")?;
    let desired_count = svc.desired_count();
    let exec_enabled = svc.enable_execute_command();

    // Get network config
    let net = svc
        .network_configuration()
        .and_then(|n| n.awsvpc_configuration());
    let subnets: Vec<String> = net
        .map(|n| n.subnets().iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let security_groups: Vec<String> = net
        .map(|n| n.security_groups().iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let assign_public_ip = net
        .and_then(|n| n.assign_public_ip())
        .map(|a| a.as_str() == "ENABLED")
        .unwrap_or(false);

    // Get capacity provider
    let capacity = svc
        .capacity_provider_strategy()
        .first()
        .map(|s| s.capacity_provider())
        .unwrap_or("FARGATE")
        .to_string();

    // Get task definition
    let td_resp = ecs
        .describe_task_definition()
        .task_definition(task_def_arn)
        .send()
        .await
        .context("DescribeTaskDefinition failed")?;

    let td = td_resp.task_definition().context("no task definition")?;
    let cpu = td.cpu().unwrap_or("256").to_string();
    let memory = td.memory().unwrap_or("512").to_string();
    let execution_role = td.execution_role_arn().map(|s| s.to_string());
    let task_role = td.task_role_arn().map(|s| s.to_string());

    let volumes: Vec<crate::apply::VolumeSpec> = td
        .volumes()
        .iter()
        .map(|v| {
            let name = v
                .name()
                .context("task definition contains a volume with no name")?
                .to_string();

            // Refuse to export what this spec cannot express, rather than
            // emitting a name-only volume that silently becomes empty
            // task-local scratch on reapply. Before this check, exporting an
            // EFS-backed service produced a spec that deployed with no EFS
            // mount at all and no indication anything was lost.
            if v.docker_volume_configuration().is_some() {
                anyhow::bail!(
                    "volume '{name}' uses dockerVolumeConfiguration, which this spec cannot represent (and which is EC2-only, not Fargate). Export refused rather than silently converting it to empty scratch storage"
                );
            }
            if v.fsx_windows_file_server_volume_configuration().is_some() {
                anyhow::bail!(
                    "volume '{name}' uses an FSx for Windows File Server configuration, which this spec cannot represent. Export refused rather than silently converting it to empty scratch storage"
                );
            }
            if let Some(host) = v.host() {
                if host.source_path().is_some() {
                    anyhow::bail!(
                        "volume '{name}' is a bind mount with a host sourcePath, which this spec cannot represent (and which is EC2-only, not Fargate). Export refused rather than silently converting it to empty scratch storage"
                    );
                }
            }
            if v.configured_at_launch() == Some(true) {
                // Carrying this through would be worse than refusing it:
                // configuredAtLaunch on the task definition is only half of an
                // EBS attachment, and this tool does not send the other half
                // (volumeConfigurations on Create/UpdateService). A reapplied
                // spec would register successfully and produce tasks with no
                // volume where the application expects one.
                anyhow::bail!(
                    "volume '{name}' is configuredAtLaunch (an EBS volume attached at deployment time), which this spec cannot represent: reapplying it would register the task definition without the volumeConfigurations needed to actually attach the volume. Export refused rather than producing a spec that deploys without the volume"
                );
            }

            let efs = match v.efs_volume_configuration() {
                None => None,
                Some(e) => {
                    if e.transit_encryption_port().is_some() {
                        anyhow::bail!(
                            "volume '{name}' sets an EFS transitEncryptionPort, which this spec cannot represent. Export refused rather than silently reverting it to the default port"
                        );
                    }
                    let auth = e.authorization_config();
                    let toggle = |v: &str| -> Result<crate::apply::EfsToggle> {
                        match v {
                            "ENABLED" => Ok(crate::apply::EfsToggle::Enabled),
                            "DISABLED" => Ok(crate::apply::EfsToggle::Disabled),
                            other => anyhow::bail!(
                                "volume '{name}' has an EFS setting this spec does not model: '{other}'"
                            ),
                        }
                    };
                    Some(crate::apply::EfsVolumeSpec {
                        file_system_id: e.file_system_id().to_string(),
                        root_directory: e.root_directory().map(|s| s.to_string()),
                        transit_encryption: e
                            .transit_encryption()
                            .map(|t| toggle(t.as_str()))
                            .transpose()?,
                        access_point_id: auth
                            .and_then(|a| a.access_point_id())
                            .map(|s| s.to_string()),
                        iam: auth
                            .and_then(|a| a.iam())
                            .map(|i| toggle(i.as_str()))
                            .transpose()?,
                    })
                }
            };

            Ok(crate::apply::VolumeSpec { name, efs })
        })
        .collect::<Result<Vec<_>>>()?;

    let arch = td
        .runtime_platform()
        .and_then(|rp| rp.cpu_architecture())
        .map(|a| a.as_str().to_string())
        .unwrap_or_else(|| "X86_64".to_string());

    // Get container definitions (skip service-connect sidecars)
    let app_containers: Vec<_> = td
        .container_definitions()
        .iter()
        .filter(|c| {
            !c.name()
                .unwrap_or_default()
                .starts_with("ecs-service-connect-")
        })
        .collect();

    let (image, container_name, port, command, env, secrets, log_group, containers) =
        if app_containers.len() > 1 {
            // Multi-container: export as containers array
            let mut cs_vec = Vec::new();
            for cd in &app_containers {
                cs_vec.push(container_to_spec(cd)?);
            }
            (
                String::new(),
                "app".to_string(),
                0u16,
                None,
                HashMap::new(),
                HashMap::new(),
                None,
                Some(cs_vec),
            )
        } else {
            // Exactly one app container. Whether this exports through the
            // legacy flat fields or the containers[] form depends on whether
            // that one container actually uses any of the fields the flat
            // form cannot represent.
            //
            // The bug this replaced: gating on `app_containers.len() > 1`
            // meant a single container using `user`, `readonlyRootFilesystem`,
            // `entryPoint`, `dependsOn`, `linuxParameters`, or `mountPoints`
            // took the flat-field branch below, which reads none of them --
            // exporting silently dropped every one of those fields. A
            // one-container hardened service (non-root, read-only rootfs,
            // dropped capabilities) would round-trip through `export` then
            // `apply` back to root, writable, full capabilities, with no
            // error and no field left to notice the loss from.
            let cd = app_containers.first().context("no app container")?;
            let uses_new_fields = container_uses_new_fields(cd);

            if uses_new_fields {
                let cs = container_to_spec(cd)?;
                (
                    String::new(),
                    "app".to_string(),
                    0u16,
                    None,
                    HashMap::new(),
                    HashMap::new(),
                    None,
                    Some(vec![cs]),
                )
            } else {
                // Original behavior: none of the new fields are in use, so the
                // flat single-container form round-trips exactly as before
                // this PR.
                let image = cd.image().unwrap_or("?").to_string();
                let cn = cd.name().unwrap_or("app").to_string();
                let port = cd
                    .port_mappings()
                    .first()
                    .map(|p| p.container_port().unwrap_or(0) as u16)
                    .unwrap_or(0);
                let command: Option<Vec<String>> = {
                    let cmds = cd.command();
                    if cmds.is_empty() {
                        None
                    } else {
                        Some(cmds.iter().map(|s| s.to_string()).collect())
                    }
                };
                let mut env: HashMap<String, String> = HashMap::new();
                for kv in cd.environment() {
                    if let (Some(k), Some(v)) = (kv.name(), kv.value()) {
                        env.insert(k.to_string(), v.to_string());
                    }
                }
                let mut secrets: HashMap<String, String> = HashMap::new();
                for s in cd.secrets() {
                    secrets.insert(s.name().to_string(), s.value_from().to_string());
                }
                let log_group = cd
                    .log_configuration()
                    .and_then(|lc| lc.options())
                    .and_then(|opts| opts.get("awslogs-group"))
                    .map(|s| s.to_string());
                (image, cn, port, command, env, secrets, log_group, None)
            }
        };

    // Build YAML
    let spec = crate::apply::ServiceSpec {
        api_version: Some("ecsctl/v1".to_string()),
        kind: Some("Service".to_string()),
        metadata: crate::apply::Metadata {
            name: service.to_string(),
            cluster: cluster.to_string(),
        },
        spec: crate::apply::Spec {
            image,
            cpu,
            memory,
            arch,
            capacity,
            desired_count,
            exec_enabled,
            env,
            secrets,
            log_group,
            execution_role_arn: execution_role,
            task_role_arn: task_role,
            subnets: Some(subnets),
            security_groups: Some(security_groups),
            assign_public_ip,
            // In multi-container mode the flat container fields are not used,
            // and apply now refuses a spec that sets them alongside
            // containers[] -- so emitting the hardcoded default here made
            // export produce a spec its own apply rejected.
            container_name: if containers.is_some() {
                None
            } else {
                Some(container_name)
            },
            port,
            command,
            containers,
            volumes,
        },
    };

    Ok(spec)
}

/// Export a service to YAML string (used by clone).
pub async fn export_to_yaml(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
) -> Result<String> {
    let target = cfg
        .aliases
        .get(name)
        .context(format!("alias '{name}' not found"))?
        .clone();

    let parts: Vec<&str> = target.splitn(4, '/').collect();
    let (cluster, service) = match parts.len() {
        2..=4 => (parts[0], parts[1]),
        _ => anyhow::bail!("invalid alias target"),
    };

    let ecs = EcsClient::new(config);
    let spec = build_spec(&ecs, cluster, service).await?;

    serde_yaml::to_string(&spec).context("failed to serialize YAML")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ecs::types::{
        ContainerCondition, ContainerDefinition, ContainerDependency, KernelCapabilities,
        LinuxParameters, MountPoint,
    };

    #[test]
    fn test_plain_container_does_not_use_new_fields() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .essential(true)
            .build();
        assert!(!container_uses_new_fields(&cd));
    }

    #[test]
    fn test_user_triggers_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .user("1000")
            .build();
        assert!(container_uses_new_fields(&cd));
    }

    #[test]
    fn test_readonly_root_filesystem_triggers_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .readonly_root_filesystem(true)
            .build();
        assert!(container_uses_new_fields(&cd));
    }

    #[test]
    fn test_entry_point_triggers_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .entry_point("/bin/sh".to_string())
            .build();
        assert!(container_uses_new_fields(&cd));
    }

    #[test]
    fn test_depends_on_triggers_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .depends_on(
                ContainerDependency::builder()
                    .container_name("init")
                    .condition(ContainerCondition::Success)
                    .build()
                    .unwrap(),
            )
            .build();
        assert!(container_uses_new_fields(&cd));
    }

    #[test]
    fn test_linux_parameters_triggers_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .linux_parameters(
                LinuxParameters::builder()
                    .capabilities(KernelCapabilities::builder().drop("ALL").build())
                    .build(),
            )
            .build();
        assert!(container_uses_new_fields(&cd));
    }

    /// The gap that let three separate round-trip breaks ship with green CI:
    /// nothing built an exported spec and ran it back through validate(). This
    /// closes it without an AWS client -- ContainerDefinition::builder() and
    /// ServiceSpec::validate() are both pure.
    #[test]
    fn exported_container_spec_round_trips_through_validate() {
        let init = ContainerDefinition::builder()
            .name("init-perms")
            .image("app:latest")
            .essential(false)
            .user("0")
            .entry_point("/bin/sh".to_string())
            .entry_point("-c".to_string())
            .command("chown 1000:1000 /workspace".to_string())
            .mount_points(
                MountPoint::builder()
                    .source_volume("workspace")
                    .container_path("/workspace")
                    .build(),
            )
            .build();

        let app = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .essential(true)
            .user("1000")
            .readonly_root_filesystem(true)
            .depends_on(
                ContainerDependency::builder()
                    .container_name("init-perms")
                    .condition(ContainerCondition::Success)
                    .build()
                    .unwrap(),
            )
            .linux_parameters(
                LinuxParameters::builder()
                    .capabilities(KernelCapabilities::builder().drop("ALL").build())
                    .build(),
            )
            .mount_points(
                MountPoint::builder()
                    .source_volume("workspace")
                    .container_path("/workspace")
                    .read_only(true)
                    .build(),
            )
            .build();

        let containers = vec![
            container_to_spec(&init).expect("init container should convert"),
            container_to_spec(&app).expect("app container should convert"),
        ];

        // Assemble the spec the way build_spec does for a multi-container
        // service, then round-trip it: serialise, parse back, validate.
        let spec = crate::apply::ServiceSpec {
            api_version: Some("ecsctl/v1".to_string()),
            kind: Some("Service".to_string()),
            metadata: crate::apply::Metadata {
                name: "svc".to_string(),
                cluster: "cl".to_string(),
            },
            spec: crate::apply::Spec {
                image: String::new(),
                cpu: "1024".to_string(),
                memory: "2048".to_string(),
                arch: "X86_64".to_string(),
                capacity: "FARGATE".to_string(),
                desired_count: 1,
                exec_enabled: false,
                env: HashMap::new(),
                secrets: HashMap::new(),
                log_group: None,
                execution_role_arn: None,
                task_role_arn: None,
                subnets: None,
                security_groups: None,
                assign_public_ip: false,
                container_name: None,
                port: 0,
                command: None,
                containers: Some(containers),
                volumes: vec![crate::apply::VolumeSpec {
                    name: "workspace".to_string(),
                    efs: None,
                }],
            },
        };

        let yaml = serde_yaml::to_string(&spec).expect("exported spec should serialise");
        let reparsed: crate::apply::ServiceSpec =
            serde_yaml::from_str(&yaml).expect("exported spec should parse back");
        reparsed
            .validate()
            .expect("an exported spec must be one apply accepts");

        // And the security-relevant details must survive the trip.
        let out = reparsed.spec.containers.as_ref().unwrap();
        let app_out = out.iter().find(|c| c.name == "app").unwrap();
        assert_eq!(app_out.user.as_deref(), Some("1000"));
        assert!(app_out.readonly_root_filesystem);
        assert_eq!(
            app_out.linux_parameters.as_ref().unwrap().capabilities_drop,
            vec!["ALL".to_string()]
        );
        assert_eq!(app_out.depends_on[0].container_name, "init-perms");
        assert!(
            app_out.mount_points.get("/workspace").unwrap().read_only(),
            "readOnly must survive export"
        );
    }

    #[test]
    fn healthy_condition_is_dropped_not_carried_into_an_unappliable_spec() {
        // Policy pinned by test: apply refuses HEALTHY, so export must not emit
        // it. Without this, a future change could carry it through and every
        // export of such a service would produce a spec that fails on reapply.
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .essential(true)
            .depends_on(
                ContainerDependency::builder()
                    .container_name("sidecar")
                    .condition(ContainerCondition::Healthy)
                    .build()
                    .unwrap(),
            )
            .build();
        let spec = container_to_spec(&cd).expect("should convert, dropping the condition");
        assert!(
            spec.depends_on.is_empty(),
            "HEALTHY must be dropped rather than emitted as something apply rejects"
        );
    }

    #[test]
    fn added_capabilities_refuse_the_export() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .linux_parameters(
                LinuxParameters::builder()
                    .capabilities(KernelCapabilities::builder().add("SYS_PTRACE").build())
                    .build(),
            )
            .build();
        assert!(
            container_to_spec(&cd).is_err(),
            "capabilities.add is not representable, so export must refuse rather than drop it"
        );
    }

    #[test]
    fn test_mount_points_trigger_containers_form() {
        let cd = ContainerDefinition::builder()
            .name("app")
            .image("app:latest")
            .mount_points(
                MountPoint::builder()
                    .source_volume("data")
                    .container_path("/data")
                    .build(),
            )
            .build();
        assert!(container_uses_new_fields(&cd));
    }
}

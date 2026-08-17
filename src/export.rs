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
        .filter_map(|v| {
            v.name().map(|n| crate::apply::VolumeSpec {
                name: n.to_string(),
            })
        })
        .collect();

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
                let mut env_map: HashMap<String, String> = HashMap::new();
                for kv in cd.environment() {
                    if let (Some(k), Some(v)) = (kv.name(), kv.value()) {
                        env_map.insert(k.to_string(), v.to_string());
                    }
                }
                let mut sec_map: HashMap<String, String> = HashMap::new();
                for s in cd.secrets() {
                    sec_map.insert(s.name().to_string(), s.value_from().to_string());
                }
                let lg = cd
                    .log_configuration()
                    .and_then(|lc| lc.options())
                    .and_then(|opts| opts.get("awslogs-group"))
                    .map(|s| s.to_string());
                let p = cd
                    .port_mappings()
                    .first()
                    .map(|p| p.container_port().unwrap_or(0) as u16)
                    .unwrap_or(0);
                let cmd: Option<Vec<String>> = {
                    let cmds = cd.command();
                    if cmds.is_empty() {
                        None
                    } else {
                        Some(cmds.iter().map(|s| s.to_string()).collect())
                    }
                };
                let entry_point: Option<Vec<String>> = {
                    let eps = cd.entry_point();
                    if eps.is_empty() {
                        None
                    } else {
                        Some(eps.iter().map(|s| s.to_string()).collect())
                    }
                };
                let depends_on: Vec<crate::apply::DependsOn> = cd
                    .depends_on()
                    .iter()
                    .map(|d| crate::apply::DependsOn {
                        container_name: d.container_name().to_string(),
                        condition: d.condition().as_str().to_string(),
                    })
                    .collect();
                let linux_parameters = cd.linux_parameters().and_then(|lp| {
                    lp.capabilities().map(|c| crate::apply::LinuxParameters {
                        capabilities_drop: c.drop().iter().map(|s| s.to_string()).collect(),
                    })
                });
                let mut mount_points: HashMap<String, String> = HashMap::new();
                for mp in cd.mount_points() {
                    if let (Some(cp), Some(sv)) = (mp.container_path(), mp.source_volume()) {
                        mount_points.insert(cp.to_string(), sv.to_string());
                    }
                }
                cs_vec.push(crate::apply::ContainerSpec {
                    name: cd.name().unwrap_or("app").to_string(),
                    image: cd.image().unwrap_or("?").to_string(),
                    essential: cd.essential().unwrap_or(true),
                    port: p,
                    command: cmd,
                    entry_point,
                    env: env_map,
                    secrets: sec_map,
                    log_group: lg,
                    user: cd.user().map(|s| s.to_string()),
                    readonly_root_filesystem: cd.readonly_root_filesystem().unwrap_or(false),
                    depends_on,
                    linux_parameters,
                    mount_points,
                });
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
                let mut env_map: HashMap<String, String> = HashMap::new();
                for kv in cd.environment() {
                    if let (Some(k), Some(v)) = (kv.name(), kv.value()) {
                        env_map.insert(k.to_string(), v.to_string());
                    }
                }
                let mut sec_map: HashMap<String, String> = HashMap::new();
                for s in cd.secrets() {
                    sec_map.insert(s.name().to_string(), s.value_from().to_string());
                }
                let lg = cd
                    .log_configuration()
                    .and_then(|lc| lc.options())
                    .and_then(|opts| opts.get("awslogs-group"))
                    .map(|s| s.to_string());
                let p = cd
                    .port_mappings()
                    .first()
                    .map(|p| p.container_port().unwrap_or(0) as u16)
                    .unwrap_or(0);
                let cmd: Option<Vec<String>> = {
                    let cmds = cd.command();
                    if cmds.is_empty() {
                        None
                    } else {
                        Some(cmds.iter().map(|s| s.to_string()).collect())
                    }
                };
                let entry_point: Option<Vec<String>> = {
                    let eps = cd.entry_point();
                    if eps.is_empty() {
                        None
                    } else {
                        Some(eps.iter().map(|s| s.to_string()).collect())
                    }
                };
                let depends_on: Vec<crate::apply::DependsOn> = cd
                    .depends_on()
                    .iter()
                    .map(|d| crate::apply::DependsOn {
                        container_name: d.container_name().to_string(),
                        condition: d.condition().as_str().to_string(),
                    })
                    .collect();
                let linux_parameters = cd.linux_parameters().and_then(|lp| {
                    lp.capabilities().map(|c| crate::apply::LinuxParameters {
                        capabilities_drop: c.drop().iter().map(|s| s.to_string()).collect(),
                    })
                });
                let mut mount_points: HashMap<String, String> = HashMap::new();
                for mp in cd.mount_points() {
                    if let (Some(cp), Some(sv)) = (mp.container_path(), mp.source_volume()) {
                        mount_points.insert(cp.to_string(), sv.to_string());
                    }
                }
                let cs = crate::apply::ContainerSpec {
                    name: cd.name().unwrap_or("app").to_string(),
                    image: cd.image().unwrap_or("?").to_string(),
                    essential: cd.essential().unwrap_or(true),
                    port: p,
                    command: cmd,
                    entry_point,
                    env: env_map,
                    secrets: sec_map,
                    log_group: lg,
                    user: cd.user().map(|s| s.to_string()),
                    readonly_root_filesystem: cd.readonly_root_filesystem().unwrap_or(false),
                    depends_on,
                    linux_parameters,
                    mount_points,
                };
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
            container_name: Some(container_name),
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

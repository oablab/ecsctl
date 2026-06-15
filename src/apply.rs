use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceSpec {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    pub cluster: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    #[serde(default = "default_essential")]
    pub essential: bool,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub log_group: Option<String>,
}

fn default_essential() -> bool { true }

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Spec {
    #[serde(default)]
    pub image: String,
    pub cpu: String,
    pub memory: String,
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(default = "default_capacity")]
    pub capacity: String,
    #[serde(default = "default_count")]
    pub desired_count: i32,
    #[serde(default)]
    pub exec_enabled: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub log_group: Option<String>,
    pub execution_role_arn: Option<String>,
    pub task_role_arn: Option<String>,
    pub subnets: Option<Vec<String>>,
    pub security_groups: Option<Vec<String>>,
    #[serde(default)]
    pub assign_public_ip: bool,
    pub container_name: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub containers: Option<Vec<ContainerSpec>>,
}

const VALID_FARGATE_SIZING: &[(u32, &[u32])] = &[
    (256, &[512, 1024, 2048]),
    (512, &[1024, 2048, 3072, 4096]),
    (1024, &[2048, 3072, 4096, 5120, 6144, 7168, 8192]),
    (2048, &[4096, 5120, 6144, 7168, 8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384]),
    (4096, &[8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384, 17408, 18432, 19456, 20480, 21504, 22528, 23552, 24576, 25600, 26624, 27648, 28672, 29696, 30720]),
];

impl ServiceSpec {
    pub fn validate(&self) -> Result<()> {
        let spec = &self.spec;

        // Validate arch
        match spec.arch.as_str() {
            "X86_64" | "ARM64" => {}
            other => anyhow::bail!("invalid arch '{}': expected X86_64 or ARM64", other),
        }

        // Validate capacity
        match spec.capacity.as_str() {
            "FARGATE" | "FARGATE_SPOT" => {}
            other => anyhow::bail!("invalid capacity '{}': expected FARGATE or FARGATE_SPOT", other),
        }

        // Validate cpu/memory combination
        let cpu: u32 = spec.cpu.parse().context("cpu must be a number (e.g. \"256\")")?;
        let mem: u32 = spec.memory.parse().context("memory must be a number (e.g. \"512\")")?;

        let valid_mems = VALID_FARGATE_SIZING
            .iter()
            .find(|(c, _)| *c == cpu)
            .map(|(_, m)| *m);

        match valid_mems {
            None => {
                let valid_cpus: Vec<_> = VALID_FARGATE_SIZING.iter().map(|(c, _)| c.to_string()).collect();
                anyhow::bail!("invalid cpu '{}': valid values are {}", cpu, valid_cpus.join(", "));
            }
            Some(mems) if !mems.contains(&mem) => {
                let opts: Vec<_> = mems.iter().map(|m| m.to_string()).collect();
                anyhow::bail!("invalid memory '{}' for cpu '{}': valid values are {}", mem, cpu, opts.join(", "));
            }
            _ => {}
        }

        // Validate desiredCount
        if spec.desired_count < 0 {
            anyhow::bail!("desiredCount must be >= 0");
        }

        Ok(())
    }
}

fn default_arch() -> String { "X86_64".to_string() }
fn default_capacity() -> String { "FARGATE".to_string() }
fn default_count() -> i32 { 1 }
fn default_port() -> u16 { 0 }

fn set_yaml_field(root: &mut serde_yaml::Value, path: &str, value: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Check original type to preserve it
            let existing = &current[*part];
            let yaml_val = if existing.is_bool() || (existing.is_null() && (value == "true" || value == "false")) {
                serde_yaml::Value::Bool(value.parse::<bool>().unwrap_or(false))
            } else if existing.is_number() {
                // Original field is a number, keep as number
                if let Ok(n) = value.parse::<i64>() {
                    serde_yaml::Value::Number(n.into())
                } else {
                    serde_yaml::Value::String(value.to_string())
                }
            } else {
                // Default: keep as string (handles cpu/memory which are string-typed numbers)
                serde_yaml::Value::String(value.to_string())
            };
            current[*part] = yaml_val;
        } else {
            current = &mut current[*part];
            if current.is_null() {
                *current = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
            }
        }
    }
    Ok(())
}

fn build_container_def(cs: &ContainerSpec, service_name: &str, region: &str) -> Result<aws_sdk_ecs::types::ContainerDefinition> {
    let mut builder = aws_sdk_ecs::types::ContainerDefinition::builder()
        .name(&cs.name)
        .image(&cs.image)
        .essential(cs.essential);

    if let Some(ref cmd) = cs.command {
        builder = builder.set_command(Some(cmd.clone()));
    }

    if cs.port > 0 {
        builder = builder.port_mappings(
            aws_sdk_ecs::types::PortMapping::builder()
                .container_port(cs.port as i32)
                .protocol(aws_sdk_ecs::types::TransportProtocol::Tcp)
                .build(),
        );
    }

    for (k, v) in &cs.env {
        builder = builder.environment(
            aws_sdk_ecs::types::KeyValuePair::builder()
                .name(k)
                .value(v)
                .build(),
        );
    }

    for (k, v) in &cs.secrets {
        builder = builder.secrets(
            aws_sdk_ecs::types::Secret::builder()
                .name(k)
                .value_from(v)
                .build()?,
        );
    }

    if let Some(ref log_group) = cs.log_group {
        builder = builder.log_configuration(
            aws_sdk_ecs::types::LogConfiguration::builder()
                .log_driver(aws_sdk_ecs::types::LogDriver::Awslogs)
                .options("awslogs-group", log_group.as_str())
                .options("awslogs-region", region)
                .options("awslogs-stream-prefix", service_name)
                .build()?,
        );
    }

    Ok(builder.build())
}

pub async fn run(config: &aws_config::SdkConfig, file: &str, overrides: &[String], wait: bool) -> Result<()> {
    let content = crate::loader::load(file).await?;
    run_from_string(config, &content, overrides, wait).await
}

/// Apply from a YAML string (used by clone).
pub async fn run_from_string(config: &aws_config::SdkConfig, content: &str, overrides: &[String], wait: bool) -> Result<()> {
    let mut yaml_value: serde_yaml::Value = serde_yaml::from_str(content).context("failed to parse YAML")?;

    // Apply --set overrides
    for entry in overrides {
        let (key, value) = entry.split_once('=').context(format!("invalid --set format '{}': expected KEY=VALUE", entry))?;
        set_yaml_field(&mut yaml_value, key, value)?;
    }

    let spec: ServiceSpec = serde_yaml::from_value(yaml_value).context("failed to parse spec after overrides")?;
    spec.validate()?;

    let ecs = EcsClient::new(config);
    let cluster = &spec.metadata.cluster;
    let service_name = &spec.metadata.name;
    let container_name = spec.spec.container_name.as_deref().unwrap_or("app");
    let family = format!("{service_name}");

    // 1. Register task definition
    eprintln!("📋 Registering task definition...");

    let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");

    let mut task_def_req = ecs
        .register_task_definition()
        .family(&family)
        .cpu(&spec.spec.cpu)
        .memory(&spec.spec.memory)
        .network_mode(aws_sdk_ecs::types::NetworkMode::Awsvpc)
        .requires_compatibilities(aws_sdk_ecs::types::Compatibility::Fargate)
        .runtime_platform(
            aws_sdk_ecs::types::RuntimePlatform::builder()
                .cpu_architecture(spec.spec.arch.as_str().into())
                .operating_system_family(aws_sdk_ecs::types::OsFamily::Linux)
                .build(),
        );

    if let Some(ref containers) = spec.spec.containers {
        // Multi-container mode
        for cs in containers {
            let cd = build_container_def(cs, service_name, region)?;
            task_def_req = task_def_req.container_definitions(cd);
        }
    } else {
        // Single-container mode (backward-compatible)
        let cs = ContainerSpec {
            name: container_name.to_string(),
            image: spec.spec.image.clone(),
            essential: true,
            port: spec.spec.port,
            command: spec.spec.command.clone(),
            env: spec.spec.env.clone(),
            secrets: spec.spec.secrets.clone(),
            log_group: spec.spec.log_group.clone(),
        };
        let cd = build_container_def(&cs, service_name, region)?;
        task_def_req = task_def_req.container_definitions(cd);
    }

    if let Some(ref role) = spec.spec.execution_role_arn {
        task_def_req = task_def_req.execution_role_arn(role);
    }
    if let Some(ref role) = spec.spec.task_role_arn {
        task_def_req = task_def_req.task_role_arn(role);
    }

    let task_def_resp = task_def_req.send().await.context("RegisterTaskDefinition failed")?;
    let task_def_arn = task_def_resp
        .task_definition()
        .and_then(|td| td.task_definition_arn())
        .context("no task definition ARN")?;
    eprintln!("  ✓ {task_def_arn}");

    // 2. Check if service exists
    let service_exists = ecs
        .describe_services()
        .cluster(cluster)
        .services(service_name)
        .send()
        .await
        .map(|r| {
            r.services()
                .first()
                .map(|s| s.status().unwrap_or_default() == "ACTIVE")
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if service_exists {
        // 3a. Update service
        eprintln!("🔄 Updating service {service_name}...");
        let mut update = ecs
            .update_service()
            .cluster(cluster)
            .service(service_name)
            .task_definition(task_def_arn)
            .desired_count(spec.spec.desired_count)
            .enable_execute_command(spec.spec.exec_enabled);

        if spec.spec.capacity == "FARGATE_SPOT" {
            update = update.capacity_provider_strategy(
                aws_sdk_ecs::types::CapacityProviderStrategyItem::builder()
                    .capacity_provider("FARGATE_SPOT")
                    .weight(1)
                    .build()?,
            );
        }

        update.send().await.context("UpdateService failed")?;
        eprintln!("  ✓ Service updated, deploying...");
    } else {
        // 3b. Create service
        eprintln!("➕ Creating service {service_name}...");

        let subnets = spec.spec.subnets.as_deref().unwrap_or_default();
        let sgs = spec.spec.security_groups.as_deref().unwrap_or_default();

        let assign_ip = if spec.spec.assign_public_ip {
            aws_sdk_ecs::types::AssignPublicIp::Enabled
        } else {
            aws_sdk_ecs::types::AssignPublicIp::Disabled
        };

        let net_config = aws_sdk_ecs::types::NetworkConfiguration::builder()
            .awsvpc_configuration(
                aws_sdk_ecs::types::AwsVpcConfiguration::builder()
                    .set_subnets(Some(subnets.iter().map(|s| s.to_string()).collect()))
                    .set_security_groups(Some(sgs.iter().map(|s| s.to_string()).collect()))
                    .assign_public_ip(assign_ip)
                    .build()?,
            )
            .build();

        let mut create = ecs
            .create_service()
            .cluster(cluster)
            .service_name(service_name)
            .task_definition(task_def_arn)
            .desired_count(spec.spec.desired_count)
            .launch_type(aws_sdk_ecs::types::LaunchType::Fargate)
            .network_configuration(net_config)
            .enable_execute_command(spec.spec.exec_enabled);

        if spec.spec.capacity == "FARGATE_SPOT" {
            // Must clear launch_type when using capacity provider
            create = create
                .set_launch_type(None)
                .capacity_provider_strategy(
                    aws_sdk_ecs::types::CapacityProviderStrategyItem::builder()
                        .capacity_provider("FARGATE_SPOT")
                        .weight(1)
                        .build()?,
                );
        }

        create.send().await.context("CreateService failed")?;
        eprintln!("  ✓ Service created");
    }

    // 4. Register alias
    let mut cfg = crate::config::Config::load()?;
    let alias_target = format!("{cluster}/{service_name}");
    if !cfg.aliases.values().any(|v| v == &alias_target) {
        cfg.aliases.insert(service_name.clone(), alias_target.clone());
        cfg.save()?;
        eprintln!("  ✓ Alias '{service_name}' → {alias_target}");
    }

    eprintln!("✓ Applied {service_name}");

    if wait {
        eprintln!("⏳ Waiting for deployment to stabilize...");
        wait_for_stable(&ecs, cluster, service_name).await?;
        eprintln!("✓ Deployment stable");
    }

    Ok(())
}

pub async fn wait_for_stable(ecs: &EcsClient, cluster: &str, service: &str) -> Result<()> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let resp = ecs
            .describe_services()
            .cluster(cluster)
            .services(service)
            .send()
            .await
            .context("DescribeServices failed")?;

        let svc = resp.services().first().context("service not found")?;
        let deployments = svc.deployments();

        if deployments.len() == 1 {
            let d = &deployments[0];
            let running = d.running_count();
            let desired = d.desired_count();
            // Only consider stable when running == desired AND desired > 0
            // (desired == 0 means scale-down — wait for rolloutState instead)
            if running == desired && desired > 0 {
                eprint!("\r  ✅ {running}/{desired} tasks running                    ");
                eprintln!();
                return Ok(());
            }
            if desired == 0 && d.rollout_state() == Some(&aws_sdk_ecs::types::DeploymentRolloutState::Completed) {
                eprint!("\r  ✅ scaled to 0 (deployment complete)                    ");
                eprintln!();
                return Ok(());
            }
            eprint!("\r  🚀 {running}/{desired} tasks running");
        } else {
            let primary = deployments.iter().find(|d| d.status().unwrap_or_default() == "PRIMARY");
            let old_count: i32 = deployments.iter()
                .filter(|d| d.status().unwrap_or_default() != "PRIMARY")
                .map(|d| d.running_count())
                .sum();
            if let Some(d) = primary {
                if d.running_count() == d.desired_count() && old_count == 0 {
                    eprint!("\r  ✅ {}/{} tasks running                    ", d.running_count(), d.desired_count());
                    eprintln!();
                    return Ok(());
                }
                eprint!("\r  🔄 new: {}/{} running, draining {} old task(s)...", d.running_count(), d.desired_count(), old_count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: test-app
  cluster: test-cluster
spec:
  image: nginx:latest
  cpu: "256"
  memory: "512"
"#
    }

    #[test]
    fn test_parse_minimal_spec() {
        let spec: ServiceSpec = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert_eq!(spec.metadata.name, "test-app");
        assert_eq!(spec.metadata.cluster, "test-cluster");
        assert_eq!(spec.spec.arch, "X86_64");
        assert_eq!(spec.spec.capacity, "FARGATE");
        assert_eq!(spec.spec.desired_count, 1);
    }

    #[test]
    fn test_validate_valid_spec() {
        let spec: ServiceSpec = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_invalid_cpu() {
        let yaml = minimal_yaml().replace("\"256\"", "\"123\"");
        let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_memory_for_cpu() {
        let yaml = minimal_yaml().replace("\"512\"", "\"8192\"");
        let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_arch() {
        let yaml = minimal_yaml().replace("cpu: \"256\"", "cpu: \"256\"\n  arch: MIPS");
        let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn test_set_yaml_field_string() {
        let mut val: serde_yaml::Value = serde_yaml::from_str(minimal_yaml()).unwrap();
        set_yaml_field(&mut val, "metadata.name", "new-name").unwrap();
        let spec: ServiceSpec = serde_yaml::from_value(val).unwrap();
        assert_eq!(spec.metadata.name, "new-name");
    }

    #[test]
    fn test_set_yaml_field_number_stays_string() {
        // cpu is a string field that holds a number
        let mut val: serde_yaml::Value = serde_yaml::from_str(minimal_yaml()).unwrap();
        set_yaml_field(&mut val, "spec.cpu", "512").unwrap();
        let spec: ServiceSpec = serde_yaml::from_value(val).unwrap();
        assert_eq!(spec.spec.cpu, "512");
    }

    #[test]
    fn test_set_yaml_field_bool() {
        let mut val: serde_yaml::Value = serde_yaml::from_str(minimal_yaml()).unwrap();
        set_yaml_field(&mut val, "spec.execEnabled", "true").unwrap();
        let spec: ServiceSpec = serde_yaml::from_value(val).unwrap();
        assert!(spec.spec.exec_enabled);
    }
}

    #[test]
    fn test_parse_multi_container_spec() {
        let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: multi-app
  cluster: test-cluster
spec:
  cpu: "512"
  memory: "1024"
  capacity: FARGATE_SPOT
  containers:
    - name: app
      image: nginx:latest
      essential: true
      port: 80
    - name: sidecar
      image: envoy:latest
      essential: false
      port: 9901
"#;
        let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.spec.containers.as_ref().unwrap().len(), 2);
        assert_eq!(spec.spec.containers.as_ref().unwrap()[0].name, "app");
        assert!(spec.spec.containers.as_ref().unwrap()[0].essential);
        assert!(!spec.spec.containers.as_ref().unwrap()[1].essential);
        assert!(spec.validate().is_ok());
    }

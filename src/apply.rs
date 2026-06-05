use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub name: String,
    pub cluster: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
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
}

fn default_arch() -> String { "X86_64".to_string() }
fn default_capacity() -> String { "FARGATE".to_string() }
fn default_count() -> i32 { 1 }
fn default_port() -> u16 { 0 }

pub async fn run(config: &aws_config::SdkConfig, file: &str) -> Result<()> {
    let content = std::fs::read_to_string(file).context("failed to read spec file")?;
    let spec: ServiceSpec = serde_yaml::from_str(&content).context("failed to parse spec")?;

    let ecs = EcsClient::new(config);
    let cluster = &spec.metadata.cluster;
    let service_name = &spec.metadata.name;
    let container_name = spec.spec.container_name.as_deref().unwrap_or("app");
    let family = format!("{service_name}");

    // 1. Register task definition
    eprintln!("📋 Registering task definition...");

    let mut container_def = aws_sdk_ecs::types::ContainerDefinition::builder()
        .name(container_name)
        .image(&spec.spec.image)
        .essential(true);

    if spec.spec.port > 0 {
        container_def = container_def.port_mappings(
            aws_sdk_ecs::types::PortMapping::builder()
                .container_port(spec.spec.port as i32)
                .protocol(aws_sdk_ecs::types::TransportProtocol::Tcp)
                .build(),
        );
    }

    for (k, v) in &spec.spec.env {
        container_def = container_def.environment(
            aws_sdk_ecs::types::KeyValuePair::builder()
                .name(k)
                .value(v)
                .build(),
        );
    }

    for (k, v) in &spec.spec.secrets {
        container_def = container_def.secrets(
            aws_sdk_ecs::types::Secret::builder()
                .name(k)
                .value_from(v)
                .build()?,
        );
    }

    if let Some(ref log_group) = spec.spec.log_group {
        let region = config.region().map(|r| r.as_ref()).unwrap_or("us-east-1");
        container_def = container_def.log_configuration(
            aws_sdk_ecs::types::LogConfiguration::builder()
                .log_driver(aws_sdk_ecs::types::LogDriver::Awslogs)
                .options("awslogs-group", log_group.as_str())
                .options("awslogs-region", region)
                .options("awslogs-stream-prefix", service_name.as_str())
                .build()?,
        );
    }

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
        )
        .container_definitions(container_def.build());

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

    eprintln!("✓ Applied {file}");
    Ok(())
}

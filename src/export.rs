use anyhow::{Context, Result};
use aws_sdk_ecs::Client as EcsClient;
use std::collections::HashMap;

use crate::config::Config;

pub async fn run(config: &aws_config::SdkConfig, name: &str, output: Option<&str>) -> Result<()> {
    let cfg = Config::load()?;
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

    // Get service details
    let svc_resp = ecs
        .describe_services()
        .cluster(cluster)
        .services(service)
        .send()
        .await
        .context("DescribeServices failed")?;

    let svc = svc_resp
        .services()
        .first()
        .context("service not found")?;

    let task_def_arn = svc.task_definition().context("no task definition")?;
    let desired_count = svc.desired_count();
    let exec_enabled = svc.enable_execute_command();

    // Get network config
    let net = svc.network_configuration().and_then(|n| n.awsvpc_configuration());
    let subnets: Vec<String> = net.map(|n| n.subnets().iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    let security_groups: Vec<String> = net.map(|n| n.security_groups().iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    let assign_public_ip = net.and_then(|n| n.assign_public_ip()).map(|a| a.as_str() == "ENABLED").unwrap_or(false);

    // Get capacity provider
    let capacity = svc.capacity_provider_strategy()
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

    let arch = td.runtime_platform()
        .and_then(|rp| rp.cpu_architecture())
        .map(|a| a.as_str().to_string())
        .unwrap_or_else(|| "X86_64".to_string());

    // Get container definition (skip sidecars)
    let cd = td.container_definitions()
        .iter()
        .find(|c| !c.name().unwrap_or_default().starts_with("ecs-service-connect-"))
        .context("no app container")?;

    let image = cd.image().unwrap_or("?").to_string();
    let container_name = cd.name().unwrap_or("app").to_string();
    let port = cd.port_mappings().first().map(|p| p.container_port().unwrap_or(0) as u16).unwrap_or(0);
    let command: Option<Vec<String>> = {
        let cmds = cd.command();
        if cmds.is_empty() { None } else { Some(cmds.iter().map(|s| s.to_string()).collect()) }
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

    let log_group = cd.log_configuration()
        .and_then(|lc| lc.options())
        .and_then(|opts| opts.get("awslogs-group"))
        .map(|s| s.to_string());

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
            desired_count: desired_count,
            exec_enabled: exec_enabled,
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
        },
    };

    let yaml = serde_yaml::to_string(&spec).context("failed to serialize YAML")?;

    match output {
        Some(out_file) => {
            std::fs::write(out_file, &yaml)?;
            eprintln!("✓ Exported {cluster}/{service} → {out_file}");
        }
        None => {
            print!("{yaml}");
        }
    }
    Ok(())
}

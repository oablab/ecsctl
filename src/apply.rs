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
pub struct DependsOn {
    pub container_name: String,
    pub condition: DependsOnCondition,
}

/// The four ECS container dependency conditions.
///
/// `Healthy` is present in the enum deliberately even though
/// `ServiceSpec::validate()` rejects it. Omitting the variant would make
/// `condition: HEALTHY` fail at serde-parse time with a generic "unknown
/// variant" message, losing the explanation of *why* it is refused (it needs a
/// `healthCheck` this spec cannot configure). Parsing it and rejecting it in
/// validation keeps that explanation while still giving every match on this
/// type exhaustiveness.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DependsOnCondition {
    Start,
    Complete,
    Success,
    Healthy,
}

impl DependsOnCondition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "START",
            Self::Complete => "COMPLETE",
            Self::Success => "SUCCESS",
            Self::Healthy => "HEALTHY",
        }
    }
}

impl std::fmt::Display for DependsOnCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every Linux capability ECS accepts in `linuxParameters.capabilities.drop`,
/// per the ECS task definition parameters reference. Validated locally because
/// a typo (`SYS_ADM` for `SYS_ADMIN`) otherwise reaches AWS and comes back as
/// a client exception naming the whole request rather than the bad value.
const VALID_CAPABILITIES: &[&str] = &[
    "ALL",
    "AUDIT_CONTROL",
    "AUDIT_WRITE",
    "BLOCK_SUSPEND",
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "IPC_LOCK",
    "IPC_OWNER",
    "KILL",
    "LEASE",
    "LINUX_IMMUTABLE",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "MKNOD",
    "NET_ADMIN",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_RAW",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_CHROOT",
    "SYS_MODULE",
    "SYS_NICE",
    "SYS_PACCT",
    "SYS_PTRACE",
    "SYS_RAWIO",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "SYSLOG",
    "WAKE_ALARM",
];

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinuxParameters {
    #[serde(default)]
    pub capabilities_drop: Vec<String>,
}

/// A container's mount of a task-level volume.
///
/// Two accepted forms, because the common case deserves the short one and
/// `readOnly` needs the long one:
///
/// ```yaml
/// mountPoints:
///   /workspace: workspace            # read-write, shorthand
///   /config:
///     sourceVolume: config
///     readOnly: true
/// ```
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum MountPointSpec {
    Volume(String),
    Detailed {
        #[serde(rename = "sourceVolume")]
        source_volume: String,
        #[serde(default, rename = "readOnly")]
        read_only: bool,
    },
}

impl MountPointSpec {
    pub fn source_volume(&self) -> &str {
        match self {
            Self::Volume(v) => v,
            Self::Detailed { source_volume, .. } => source_volume,
        }
    }

    pub fn read_only(&self) -> bool {
        match self {
            Self::Volume(_) => false,
            Self::Detailed { read_only, .. } => *read_only,
        }
    }
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
    pub entry_point: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub log_group: Option<String>,
    /// Container-level user override (e.g. "0" for root, "1000" for a fixed uid).
    /// Sidecars that need root for one-shot setup (chown, tailscaled) while the
    /// app container stays non-root need this per-container, not just at the task
    /// level, which ECS does not support anyway (there is no task-level "user").
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub readonly_root_filesystem: bool,
    /// Startup ordering: this container waits for the named one to reach
    /// `condition` (START, SUCCESS, or COMPLETE) before starting. `HEALTHY`
    /// parses but is refused by validation — see `DependsOnCondition`.
    ///
    /// Without this field at all, an init container that must run and exit
    /// before the app container starts (e.g. chowning a shared volume so a
    /// non-root container can write to it) has no way to express that ordering
    /// — ECS starts every container in a task concurrently by default.
    #[serde(default)]
    pub depends_on: Vec<DependsOn>,
    #[serde(default)]
    pub linux_parameters: Option<LinuxParameters>,
    /// Shared-volume mount points, keyed by container path. Keying by path
    /// rather than using a list makes a duplicate mount path unrepresentable,
    /// and BTreeMap keeps the registered task definition's mount order stable
    /// across runs for the same spec.
    #[serde(default)]
    pub mount_points: std::collections::BTreeMap<String, MountPointSpec>,
}

fn default_essential() -> bool {
    true
}

/// EFS configuration for a task-level volume. EFS is the only non-ephemeral
/// volume type usable on Fargate (`host` and `dockerVolumeConfiguration` are
/// EC2-only, and this tool registers Fargate task definitions), so it is the
/// only one modelled here. Anything else found while exporting is refused
/// loudly rather than silently flattened to an empty scratch volume.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EfsVolumeSpec {
    pub file_system_id: String,
    #[serde(default)]
    pub root_directory: Option<String>,
    #[serde(default)]
    pub transit_encryption: Option<String>,
    #[serde(default)]
    pub access_point_id: Option<String>,
    #[serde(default)]
    pub iam: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VolumeSpec {
    pub name: String,
    /// Omitted for an ephemeral, ECS-managed scratch volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efs: Option<EfsVolumeSpec>,
    /// Deferred configuration at launch, required for attaching an EBS volume.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub configured_at_launch: bool,
}

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
    /// Task-level shared volumes (ephemeral, ECS-managed — not EFS or host path).
    /// Referenced by name from each container's `mountPoints`.
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
}

const VALID_FARGATE_SIZING: &[(u32, &[u32])] = &[
    (256, &[512, 1024, 2048]),
    (512, &[1024, 2048, 3072, 4096]),
    (1024, &[2048, 3072, 4096, 5120, 6144, 7168, 8192]),
    (
        2048,
        &[
            4096, 5120, 6144, 7168, 8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384,
        ],
    ),
    (
        4096,
        &[
            8192, 9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384, 17408, 18432, 19456,
            20480, 21504, 22528, 23552, 24576, 25600, 26624, 27648, 28672, 29696, 30720,
        ],
    ),
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
            other => anyhow::bail!(
                "invalid capacity '{}': expected FARGATE or FARGATE_SPOT",
                other
            ),
        }

        // Validate cpu/memory combination
        let cpu: u32 = spec
            .cpu
            .parse()
            .context("cpu must be a number (e.g. \"256\")")?;
        let mem: u32 = spec
            .memory
            .parse()
            .context("memory must be a number (e.g. \"512\")")?;

        let valid_mems = VALID_FARGATE_SIZING
            .iter()
            .find(|(c, _)| *c == cpu)
            .map(|(_, m)| *m);

        match valid_mems {
            None => {
                let valid_cpus: Vec<_> = VALID_FARGATE_SIZING
                    .iter()
                    .map(|(c, _)| c.to_string())
                    .collect();
                anyhow::bail!(
                    "invalid cpu '{}': valid values are {}",
                    cpu,
                    valid_cpus.join(", ")
                );
            }
            Some(mems) if !mems.contains(&mem) => {
                let opts: Vec<_> = mems.iter().map(|m| m.to_string()).collect();
                anyhow::bail!(
                    "invalid memory '{}' for cpu '{}': valid values are {}",
                    mem,
                    cpu,
                    opts.join(", ")
                );
            }
            _ => {}
        }

        // Validate desiredCount
        if spec.desired_count < 0 {
            anyhow::bail!("desiredCount must be >= 0");
        }

        // Validate multi-container cross-references and dependsOn conditions
        // locally, before anything is sent to AWS. Each of these previously
        // parsed and validated cleanly, then failed at RegisterTaskDefinition
        // with an AWS-worded error unrelated to the line that caused it.
        if let Some(ref containers) = spec.containers {
            if containers.is_empty() {
                anyhow::bail!(
                    "spec.containers is present but empty: specify at least one container, or omit spec.containers to use single-container mode"
                );
            }

            // Two containers sharing a name collide in the registered task
            // definition. A HashSet silently absorbs the duplicate, so check
            // by comparing lengths rather than just building one.
            {
                let mut seen = std::collections::HashSet::with_capacity(containers.len());
                for cs in containers {
                    if !seen.insert(cs.name.as_str()) {
                        anyhow::bail!(
                            "container name '{}' is used by more than one entry in spec.containers",
                            cs.name
                        );
                    }
                }
            }

            // Same check for volumes: a duplicate name means mountPoints
            // referencing it resolve ambiguously, and AWS's own error would
            // not explain why.
            {
                let mut seen = std::collections::HashSet::with_capacity(spec.volumes.len());
                for vol in &spec.volumes {
                    if !seen.insert(vol.name.as_str()) {
                        anyhow::bail!(
                            "volume name '{}' is used by more than one entry in spec.volumes",
                            vol.name
                        );
                    }
                }
            }

            // AWS requires at least one essential container per task; an
            // all-essential:false spec parses and passes every check above,
            // then fails at RegisterTaskDefinition.
            if !containers.iter().any(|c| c.essential) {
                anyhow::bail!(
                    "spec.containers has no essential container: every task needs at least one (essential defaults to true, so this means every container explicitly sets essential: false)"
                );
            }

            let container_names: std::collections::HashSet<&str> =
                containers.iter().map(|c| c.name.as_str()).collect();
            let essential_by_name: std::collections::HashMap<&str, bool> = containers
                .iter()
                .map(|c| (c.name.as_str(), c.essential))
                .collect();
            let volume_names: std::collections::HashSet<&str> =
                spec.volumes.iter().map(|v| v.name.as_str()).collect();

            for cs in containers {
                if let Some(ref lp) = cs.linux_parameters {
                    for cap in &lp.capabilities_drop {
                        if !VALID_CAPABILITIES.contains(&cap.as_str()) {
                            anyhow::bail!(
                                "container '{}' drops unknown Linux capability '{}': ECS accepts only the capability names in its task definition reference (ALL, SYS_ADMIN, NET_RAW, ...), and a typo here is rejected by AWS with an error that does not name the bad value",
                                cs.name,
                                cap
                            );
                        }
                    }
                }

                for dep in &cs.depends_on {
                    // Existence and self-reference first: these are true
                    // regardless of condition, and checking them after the
                    // condition-specific rule below meant a self-dependency
                    // with condition: SUCCESS on an essential container hit
                    // the essential-target error first ("set essential:
                    // false"), which is misleading advice for a spec that is
                    // also invalid for an unrelated reason -- the two errors
                    // would surface one at a time across two fix attempts.
                    if !container_names.contains(dep.container_name.as_str()) {
                        anyhow::bail!(
                            "container '{}' dependsOn references '{}', which is not defined in spec.containers",
                            cs.name,
                            dep.container_name
                        );
                    }
                    if dep.container_name == cs.name {
                        anyhow::bail!("container '{}' has a dependsOn on itself", cs.name);
                    }

                    // Verified against AWS's own reported error text (the docs
                    // page states the restriction but not which side of the
                    // dependency it binds): "A dependency container with
                    // SUCCESS or COMPLETE condition cannot be an essential
                    // container" -- the restriction is on dep.container_name
                    // (the target being waited on), not on cs (the container
                    // declaring the dependsOn). An init container with
                    // essential: false being awaited by an essential app
                    // container -- this crate's motivating case -- is legal;
                    // it is the init container itself that must be
                    // essential: false.
                    let target_essential = essential_by_name.get(dep.container_name.as_str());
                    match dep.condition {
                        DependsOnCondition::Start => {}
                        DependsOnCondition::Complete | DependsOnCondition::Success
                            if target_essential == Some(&true) =>
                        {
                            anyhow::bail!(
                                "container '{}' dependsOn '{}' with condition {}, but '{}' is essential: true. AWS rejects this combination (\"A dependency container with SUCCESS or COMPLETE condition cannot be an essential container\") -- set essential: false on '{}', or use condition: START",
                                cs.name,
                                dep.container_name,
                                dep.condition,
                                dep.container_name,
                                dep.container_name
                            )
                        }
                        DependsOnCondition::Complete | DependsOnCondition::Success => {}
                        DependsOnCondition::Healthy => anyhow::bail!(
                            "container '{}' dependsOn '{}' uses condition HEALTHY, which is not yet supported: it requires a healthCheck on '{}' that this spec has no way to configure, and would fail at RegisterTaskDefinition rather than here. Use START, SUCCESS, or COMPLETE instead",
                            cs.name,
                            dep.container_name,
                            dep.container_name
                        ),
                    }
                }

                // mount_points is a BTreeMap, so iteration order — and hence
                // which offending mount is reported first — is stable for a
                // given spec.
                for (container_path, mp) in &cs.mount_points {
                    if !volume_names.contains(mp.source_volume()) {
                        anyhow::bail!(
                            "container '{}' mounts '{}' at '{}', but '{}' is not defined in spec.volumes",
                            cs.name,
                            mp.source_volume(),
                            container_path,
                            mp.source_volume()
                        );
                    }
                }
            }

            // Cycle detection. Self-reference is caught above; this catches
            // the longer loops (A waits on B, B waits on A), which ECS also
            // rejects but only at RegisterTaskDefinition, and which are much
            // harder to spot by eye than a self-reference once there are more
            // than three containers.
            {
                let edges: std::collections::BTreeMap<&str, Vec<&str>> = containers
                    .iter()
                    .map(|c| {
                        (
                            c.name.as_str(),
                            c.depends_on
                                .iter()
                                .map(|d| d.container_name.as_str())
                                .collect(),
                        )
                    })
                    .collect();

                #[derive(Clone, Copy, PartialEq)]
                enum Mark {
                    Open,
                    Done,
                }

                // Iterative DFS with an explicit path, so the error can name
                // the actual cycle rather than just asserting one exists.
                let mut marks: std::collections::BTreeMap<&str, Mark> =
                    std::collections::BTreeMap::new();
                for root in edges.keys() {
                    if marks.get(root) == Some(&Mark::Done) {
                        continue;
                    }
                    let mut path: Vec<&str> = Vec::new();
                    // (node, next-child-index)
                    let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
                    marks.insert(root, Mark::Open);
                    path.push(root);

                    while let Some((node, child_idx)) = stack.pop() {
                        let children = edges.get(node).map(|v| v.as_slice()).unwrap_or(&[]);
                        if child_idx < children.len() {
                            stack.push((node, child_idx + 1));
                            let child = children[child_idx];
                            match marks.get(child) {
                                Some(Mark::Open) => {
                                    // Found a back edge: the cycle is the tail
                                    // of the current path starting at `child`.
                                    let start = path.iter().position(|n| *n == child).unwrap_or(0);
                                    let mut cycle: Vec<&str> = path[start..].to_vec();
                                    cycle.push(child);
                                    anyhow::bail!(
                                        "dependsOn cycle in spec.containers: {}. ECS cannot start any container in a cycle, and rejects this at RegisterTaskDefinition",
                                        cycle.join(" -> ")
                                    );
                                }
                                Some(Mark::Done) => {}
                                None => {
                                    marks.insert(child, Mark::Open);
                                    path.push(child);
                                    stack.push((child, 0));
                                }
                            }
                        } else {
                            marks.insert(node, Mark::Done);
                            path.pop();
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn default_arch() -> String {
    "X86_64".to_string()
}
fn default_capacity() -> String {
    "FARGATE".to_string()
}
fn default_count() -> i32 {
    1
}
fn default_port() -> u16 {
    0
}

fn set_yaml_field(root: &mut serde_yaml::Value, path: &str, value: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Check original type to preserve it
            let existing = &current[*part];
            let yaml_val = if existing.is_bool()
                || (existing.is_null() && (value == "true" || value == "false"))
            {
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

fn build_container_def(
    cs: &ContainerSpec,
    service_name: &str,
    region: &str,
) -> Result<aws_sdk_ecs::types::ContainerDefinition> {
    let mut builder = aws_sdk_ecs::types::ContainerDefinition::builder()
        .name(&cs.name)
        .image(&cs.image)
        .essential(cs.essential);

    if let Some(ref cmd) = cs.command {
        builder = builder.set_command(Some(cmd.clone()));
    }

    if let Some(ref ep) = cs.entry_point {
        builder = builder.set_entry_point(Some(ep.clone()));
    }

    if let Some(ref user) = cs.user {
        builder = builder.user(user);
    }

    if cs.readonly_root_filesystem {
        builder = builder.readonly_root_filesystem(true);
    }

    for dep in &cs.depends_on {
        let condition = match dep.condition {
            DependsOnCondition::Start => aws_sdk_ecs::types::ContainerCondition::Start,
            DependsOnCondition::Success => aws_sdk_ecs::types::ContainerCondition::Success,
            DependsOnCondition::Complete => aws_sdk_ecs::types::ContainerCondition::Complete,
            // Rejected in ServiceSpec::validate() before this runs on the
            // apply path; this arm keeps the match exhaustive and is a
            // defensive stop for direct callers that bypass validate().
            DependsOnCondition::Healthy => anyhow::bail!(
                "container '{}' dependsOn '{}' uses condition HEALTHY, which is not yet supported (no way to configure the required healthCheck)",
                cs.name,
                dep.container_name
            ),
        };
        builder = builder.depends_on(
            aws_sdk_ecs::types::ContainerDependency::builder()
                .container_name(&dep.container_name)
                .condition(condition)
                .build()?,
        );
    }

    if let Some(ref lp) = cs.linux_parameters {
        builder = builder.linux_parameters(
            aws_sdk_ecs::types::LinuxParameters::builder()
                .capabilities(
                    aws_sdk_ecs::types::KernelCapabilities::builder()
                        .set_drop(Some(lp.capabilities_drop.clone()))
                        .build(),
                )
                .build(),
        );
    }

    for (container_path, mp) in &cs.mount_points {
        builder = builder.mount_points(
            aws_sdk_ecs::types::MountPoint::builder()
                .source_volume(mp.source_volume())
                .container_path(container_path)
                .read_only(mp.read_only())
                .build(),
        );
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

pub async fn run(
    config: &aws_config::SdkConfig,
    file: &str,
    overrides: &[String],
    wait: bool,
) -> Result<()> {
    let content = crate::loader::load(file).await?;
    run_from_string(config, &content, overrides, wait).await
}

/// Apply from a YAML string (used by clone).
pub async fn run_from_string(
    config: &aws_config::SdkConfig,
    content: &str,
    overrides: &[String],
    wait: bool,
) -> Result<()> {
    let mut yaml_value: serde_yaml::Value =
        serde_yaml::from_str(content).context("failed to parse YAML")?;

    // Apply --set overrides
    for entry in overrides {
        let (key, value) = entry.split_once('=').context(format!(
            "invalid --set format '{}': expected KEY=VALUE",
            entry
        ))?;
        set_yaml_field(&mut yaml_value, key, value)?;
    }

    let spec: ServiceSpec =
        serde_yaml::from_value(yaml_value).context("failed to parse spec after overrides")?;
    spec.validate()?;

    let ecs = EcsClient::new(config);
    let cluster = &spec.metadata.cluster;
    let service_name = &spec.metadata.name;
    let container_name = spec.spec.container_name.as_deref().unwrap_or("app");
    let family = service_name.to_string();

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
            entry_point: None,
            env: spec.spec.env.clone(),
            secrets: spec.spec.secrets.clone(),
            log_group: spec.spec.log_group.clone(),
            user: None,
            readonly_root_filesystem: false,
            depends_on: Vec::new(),
            linux_parameters: None,
            mount_points: std::collections::BTreeMap::new(),
        };
        let cd = build_container_def(&cs, service_name, region)?;
        task_def_req = task_def_req.container_definitions(cd);
    }

    if !spec.spec.volumes.is_empty() {
        for vol in &spec.spec.volumes {
            let mut vb = aws_sdk_ecs::types::Volume::builder().name(&vol.name);
            if vol.configured_at_launch {
                vb = vb.configured_at_launch(true);
            }
            if let Some(ref efs) = vol.efs {
                let mut eb = aws_sdk_ecs::types::EfsVolumeConfiguration::builder()
                    .file_system_id(&efs.file_system_id);
                if let Some(ref rd) = efs.root_directory {
                    eb = eb.root_directory(rd);
                }
                if let Some(ref te) = efs.transit_encryption {
                    eb = eb.transit_encryption(te.as_str().into());
                }
                if efs.access_point_id.is_some() || efs.iam.is_some() {
                    let mut ab = aws_sdk_ecs::types::EfsAuthorizationConfig::builder();
                    if let Some(ref ap) = efs.access_point_id {
                        ab = ab.access_point_id(ap);
                    }
                    if let Some(ref iam) = efs.iam {
                        ab = ab.iam(iam.as_str().into());
                    }
                    eb = eb.authorization_config(ab.build());
                }
                vb = vb.efs_volume_configuration(eb.build()?);
            }
            task_def_req = task_def_req.volumes(vb.build());
        }
    }

    if let Some(ref role) = spec.spec.execution_role_arn {
        task_def_req = task_def_req.execution_role_arn(role);
    }
    if let Some(ref role) = spec.spec.task_role_arn {
        task_def_req = task_def_req.task_role_arn(role);
    }

    let task_def_resp = task_def_req
        .send()
        .await
        .context("RegisterTaskDefinition failed")?;
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
            create = create.set_launch_type(None).capacity_provider_strategy(
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
        cfg.aliases
            .insert(service_name.clone(), alias_target.clone());
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
            if desired == 0
                && d.rollout_state() == Some(&aws_sdk_ecs::types::DeploymentRolloutState::Completed)
            {
                eprint!("\r  ✅ scaled to 0 (deployment complete)                    ");
                eprintln!();
                return Ok(());
            }
            eprint!("\r  🚀 {running}/{desired} tasks running");
        } else {
            let primary = deployments
                .iter()
                .find(|d| d.status().unwrap_or_default() == "PRIMARY");
            let old_count: i32 = deployments
                .iter()
                .filter(|d| d.status().unwrap_or_default() != "PRIMARY")
                .map(|d| d.running_count())
                .sum();
            if let Some(d) = primary {
                if d.running_count() == d.desired_count() && old_count == 0 {
                    eprint!(
                        "\r  ✅ {}/{} tasks running                    ",
                        d.running_count(),
                        d.desired_count()
                    );
                    eprintln!();
                    return Ok(());
                }
                eprint!(
                    "\r  🔄 new: {}/{} running, draining {} old task(s)...",
                    d.running_count(),
                    d.desired_count(),
                    old_count
                );
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

/// Mirrors the shape a tailscale-sidecar-fronted service actually needs: an
/// init container that chowns a shared volume and must finish (SUCCESS) before
/// the non-root app container starts, plus root-vs-non-root user overrides,
/// readonly rootfs, and dropped capabilities on the app container.
#[test]
fn test_parse_sidecar_spec_with_depends_on_and_volumes() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: sidecar-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: workspace
  containers:
    - name: init-perms
      image: app:latest
      essential: false
      user: "0"
      entryPoint: ["/bin/sh", "-c"]
      command: ["chown 1000:1000 /workspace"]
      mountPoints:
        /workspace: workspace
    - name: app
      image: app:latest
      essential: true
      user: "1000"
      readonlyRootFilesystem: true
      dependsOn:
        - containerName: init-perms
          condition: SUCCESS
      linuxParameters:
        capabilitiesDrop: ["ALL"]
      mountPoints:
        /workspace: workspace
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_ok());

    let containers = spec.spec.containers.as_ref().unwrap();
    assert_eq!(spec.spec.volumes.len(), 1);
    assert_eq!(spec.spec.volumes[0].name, "workspace");

    let init = &containers[0];
    assert_eq!(init.user.as_deref(), Some("0"));
    assert_eq!(
        init.entry_point.as_ref().unwrap(),
        &vec!["/bin/sh".to_string(), "-c".to_string()]
    );
    assert_eq!(
        init.mount_points.get("/workspace").unwrap().source_volume(),
        "workspace"
    );
    assert!(!init.mount_points.get("/workspace").unwrap().read_only());

    let app = &containers[1];
    assert_eq!(app.user.as_deref(), Some("1000"));
    assert!(app.readonly_root_filesystem);
    assert_eq!(app.depends_on.len(), 1);
    assert_eq!(app.depends_on[0].container_name, "init-perms");
    assert_eq!(app.depends_on[0].condition, DependsOnCondition::Success);
    assert_eq!(
        app.linux_parameters.as_ref().unwrap().capabilities_drop,
        vec!["ALL".to_string()]
    );
}

#[test]
fn test_backward_compatible_single_container_still_parses() {
    // The single-container path must keep working unchanged: none of the new
    // fields are required, and an old spec with none of them present must still
    // parse and validate exactly as before.
    let spec: ServiceSpec = serde_yaml::from_str(
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
"#,
    )
    .unwrap();
    assert!(spec.spec.containers.is_none());
    assert!(spec.spec.volumes.is_empty());
    assert!(spec.validate().is_ok());
}

#[test]
fn test_validate_rejects_success_condition_on_essential_target() {
    // The restriction binds the target of dependsOn (the container being
    // waited on), not the container declaring dependsOn -- confirmed against
    // AWS's actual reported error text ("A dependency container with SUCCESS
    // or COMPLETE condition cannot be an essential container"), not just the
    // docs page's ambiguous wording.
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: sidecar-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: app
      image: app:latest
      essential: true
      dependsOn:
        - containerName: envoy
          condition: SUCCESS
    - name: envoy
      image: envoy:latest
      essential: true
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("envoy"), "error was: {err}");
    assert!(err.contains("essential"), "error was: {err}");
}

#[test]
fn test_validate_accepts_success_condition_on_nonessential_target() {
    // The motivating shape: an essential app container waits for a
    // non-essential init container to SUCCEED. This must stay legal --
    // it's the exact case this PR exists to support.
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: sidecar-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: app
      image: app:latest
      essential: true
      dependsOn:
        - containerName: init-perms
          condition: SUCCESS
    - name: init-perms
      image: app:latest
      essential: false
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_ok());
}

#[test]
fn test_validate_rejects_duplicate_container_names() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: dup-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: app
      image: one:latest
    - name: app
      image: two:latest
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("app"), "error was: {err}");
}

#[test]
fn test_validate_rejects_empty_containers_list() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: empty-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers: []
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_err());
}

#[test]
fn test_validate_rejects_no_essential_container() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: all-nonessential
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: a
      image: a:latest
      essential: false
    - name: b
      image: b:latest
      essential: false
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("essential"), "error was: {err}");
}

#[test]
fn test_validate_rejects_duplicate_volume_names() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: dup-volume
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: data
    - name: data
  containers:
    - name: app
      image: app:latest
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("data"), "error was: {err}");
}

#[test]
fn test_validate_self_dependency_reported_over_essential_target() {
    // A container that is essential:true, depends on itself, with condition
    // SUCCESS, hits two rules at once: self-dependency (always wrong) and
    // essential-target (wrong only because the target happens to be itself,
    // which is essential:true here). Self-reference must be reported --
    // "set essential: false" would be actively wrong advice for a
    // self-dependency, since making the container non-essential does not
    // make depending on itself valid.
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: self-dep
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: app
      image: app:latest
      essential: true
      dependsOn:
        - containerName: app
          condition: SUCCESS
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("itself"), "error was: {err}");
    assert!(
        !err.contains("essential: true"),
        "reported the essential-target error instead of self-dependency: {err}"
    );
}

#[test]
fn test_unknown_condition_now_fails_at_parse_not_validate() {
    // With condition typed as an enum, an unknown value is refused by serde
    // before validate() ever runs. The error names the field and the bad
    // value, which is what mattered about the old validate()-level check.
    let yaml = sidecar_yaml_with(
        "dependsOn:\n        - containerName: init-perms\n          condition: BOGUS",
    );
    let err = serde_yaml::from_str::<ServiceSpec>(&yaml)
        .unwrap_err()
        .to_string();
    assert!(err.contains("BOGUS"), "error was: {err}");
}

#[test]
fn test_validate_rejects_unknown_capability() {
    let yaml = sidecar_yaml_with("linuxParameters:\n        capabilitiesDrop: [\"SYS_ADM\"]");
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("SYS_ADM"), "error was: {err}");
}

#[test]
fn test_validate_accepts_known_capabilities() {
    let yaml =
        sidecar_yaml_with("linuxParameters:\n        capabilitiesDrop: [\"ALL\", \"SYS_ADMIN\"]");
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    assert!(spec.validate().is_ok());
}

#[test]
fn test_validate_detects_dependency_cycle() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: cyclic
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: a
      image: a:latest
      essential: true
      dependsOn:
        - containerName: b
          condition: START
    - name: b
      image: b:latest
      essential: false
      dependsOn:
        - containerName: a
          condition: START
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("cycle"), "error was: {err}");
    // The message should name the actual loop, not just assert one exists.
    assert!(err.contains("a") && err.contains("b"), "error was: {err}");
}

#[test]
fn test_validate_accepts_diamond_dependency_without_cycle() {
    // Two containers both waiting on the same init container is a DAG, not a
    // cycle -- a naive "visited" check would wrongly reject this.
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: diamond
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  containers:
    - name: init
      image: init:latest
      essential: false
    - name: a
      image: a:latest
      essential: true
      dependsOn:
        - containerName: init
          condition: SUCCESS
    - name: b
      image: b:latest
      essential: true
      dependsOn:
        - containerName: init
          condition: SUCCESS
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_ok());
}

#[test]
fn test_mount_point_read_only_forms_parse() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: mounts
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: workspace
    - name: config
  containers:
    - name: app
      image: app:latest
      essential: true
      mountPoints:
        /workspace: workspace
        /config:
          sourceVolume: config
          readOnly: true
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_ok());
    let app = &spec.spec.containers.as_ref().unwrap()[0];
    let ws = app.mount_points.get("/workspace").unwrap();
    assert_eq!(ws.source_volume(), "workspace");
    assert!(!ws.read_only(), "shorthand form must default to read-write");
    let cfg = app.mount_points.get("/config").unwrap();
    assert_eq!(cfg.source_volume(), "config");
    assert!(cfg.read_only());
}

#[test]
fn test_efs_volume_parses_and_validates() {
    let yaml = r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: efs-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: shared
      efs:
        fileSystemId: fs-0123456789abcdef0
        rootDirectory: /data
        transitEncryption: ENABLED
  containers:
    - name: app
      image: app:latest
      essential: true
      mountPoints:
        /data: shared
"#;
    let spec: ServiceSpec = serde_yaml::from_str(yaml).unwrap();
    assert!(spec.validate().is_ok());
    let efs = spec.spec.volumes[0].efs.as_ref().unwrap();
    assert_eq!(efs.file_system_id, "fs-0123456789abcdef0");
    assert_eq!(efs.root_directory.as_deref(), Some("/data"));
}

#[cfg(test)]
fn sidecar_yaml_with(extra_container_field: &str) -> String {
    format!(
        r#"
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: sidecar-app
  cluster: test-cluster
spec:
  cpu: "1024"
  memory: "2048"
  volumes:
    - name: workspace
  containers:
    - name: init-perms
      image: app:latest
      essential: false
      mountPoints:
        /workspace: workspace
    - name: app
      image: app:latest
      essential: true
      {extra}
"#,
        extra = extra_container_field
    )
}

#[test]
fn test_validate_rejects_healthy_condition() {
    let yaml = sidecar_yaml_with(
        "dependsOn:\n        - containerName: init-perms\n          condition: HEALTHY",
    );
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("HEALTHY"), "error was: {err}");
    assert!(err.contains("healthCheck"), "error was: {err}");
}

#[test]
fn test_validate_rejects_depends_on_unknown_container() {
    let yaml = sidecar_yaml_with(
        "dependsOn:\n        - containerName: does-not-exist\n          condition: SUCCESS",
    );
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("does-not-exist"), "error was: {err}");
}

#[test]
fn test_validate_rejects_depends_on_self() {
    let yaml =
        sidecar_yaml_with("dependsOn:\n        - containerName: app\n          condition: SUCCESS");
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    // This case is also essential:true depending on itself with SUCCESS,
    // which would also trip the essential-target check. Asserting the
    // message pins down that it fails for self-reference specifically (which
    // is now checked first) rather than passing for an unrelated reason --
    // a bare is_err() would not catch the check-ordering bug this exact case
    // exposed during review (the essential-target error fired first and
    // told the user to "set essential: false", which does not fix a
    // self-dependency).
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("itself"), "error was: {err}");
}

#[test]
fn test_validate_rejects_mount_point_unknown_volume() {
    let yaml = sidecar_yaml_with("mountPoints:\n        /data: not-a-real-volume");
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    let err = spec.validate().unwrap_err().to_string();
    assert!(err.contains("not-a-real-volume"), "error was: {err}");
}

#[test]
fn test_validate_accepts_valid_depends_on_and_mount_points() {
    let yaml = sidecar_yaml_with(
        "dependsOn:\n        - containerName: init-perms\n          condition: SUCCESS\n      mountPoints:\n        /workspace: workspace",
    );
    let spec: ServiceSpec = serde_yaml::from_str(&yaml).unwrap();
    assert!(spec.validate().is_ok());
}

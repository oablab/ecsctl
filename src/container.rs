use aws_sdk_ecs::types::ContainerDefinition;

/// Known infrastructure container name prefixes to skip.
const INFRA_PREFIXES: &[&str] = &[
    "ecs-service-connect-",
    "aws-guardduty-agent",
    "datadog-agent",
    "xray-daemon",
    "envoy",
];

/// Find the main application container from a task definition's container list.
///
/// Priority:
/// 1. Single essential container (excluding infra)
/// 2. Essential container with log configuration
/// 3. First non-infra, non-init container (has no dependsOn with SUCCESS condition)
/// 4. First non-infra container
pub fn find_main_container(containers: &[ContainerDefinition]) -> Option<&ContainerDefinition> {
    let app_containers: Vec<_> = containers
        .iter()
        .filter(|cd| !is_infra_container(cd))
        .collect();

    if app_containers.is_empty() {
        return containers.first();
    }

    // If only one non-infra container, that's it
    if app_containers.len() == 1 {
        return Some(app_containers[0]);
    }

    // Prefer essential container with log config
    let essential_with_logs = app_containers
        .iter()
        .find(|cd| cd.essential().unwrap_or(false) && cd.log_configuration().is_some());
    if let Some(cd) = essential_with_logs {
        return Some(cd);
    }

    // Prefer any essential container
    let essential = app_containers
        .iter()
        .find(|cd| cd.essential().unwrap_or(false));
    if let Some(cd) = essential {
        return Some(cd);
    }

    // Prefer container without dependsOn (not an init container)
    let non_init = app_containers.iter().find(|cd| cd.depends_on().is_empty());
    if let Some(cd) = non_init {
        return Some(cd);
    }

    Some(app_containers[0])
}

fn is_infra_container(cd: &ContainerDefinition) -> bool {
    let name = cd.name().unwrap_or_default();
    INFRA_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_container(
        name: &str,
        essential: bool,
        has_log: bool,
        has_depends: bool,
    ) -> ContainerDefinition {
        let mut builder = ContainerDefinition::builder()
            .name(name)
            .essential(essential);
        if has_log {
            builder = builder.log_configuration(
                aws_sdk_ecs::types::LogConfiguration::builder()
                    .log_driver(aws_sdk_ecs::types::LogDriver::Awslogs)
                    .build()
                    .unwrap(),
            );
        }
        if has_depends {
            builder = builder.depends_on(
                aws_sdk_ecs::types::ContainerDependency::builder()
                    .container_name("main")
                    .condition(aws_sdk_ecs::types::ContainerCondition::Success)
                    .build()
                    .unwrap(),
            );
        }
        builder.build()
    }

    #[test]
    fn picks_essential_over_sidecar() {
        let containers = vec![
            make_container("s3-restore", false, true, false),
            make_container("app", true, true, false),
            make_container("s3-sync", false, true, false),
        ];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("app"));
    }

    #[test]
    fn skips_infra_containers() {
        let containers = vec![
            make_container("ecs-service-connect-proxy", true, true, false),
            make_container("myapp", true, true, false),
        ];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("myapp"));
    }

    #[test]
    fn single_container() {
        let containers = vec![make_container("worker", true, true, false)];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("worker"));
    }

    #[test]
    fn prefers_non_init_when_no_essential() {
        let containers = vec![
            make_container("init", false, true, true),
            make_container("app", false, true, false),
        ];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("app"));
    }
}

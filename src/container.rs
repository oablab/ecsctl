use aws_sdk_ecs::types::ContainerDefinition;

/// Known infrastructure container name prefixes to skip.
const INFRA_PREFIXES: &[&str] = &[
    "ecs-service-connect-",
    "aws-guardduty-agent",
    "datadog-agent",
    "xray-daemon",
    "envoy",
    // Network sidecars that share the task's namespace to make the app
    // reachable. Found by deploying a real three-container task: exec resolved
    // to the tailscale sidecar rather than the application, because the sidecar
    // is also essential and also has a log configuration, and happened to come
    // first in the container list.
    "tailscale",
    "cloudflared",
    "ts-sidecar",
];

/// Find the main application container from a task definition's container list.
///
/// Priority:
/// 1. Single non-infra container
/// 2. Essential container that declares `dependsOn` — in the init-container
///    pattern the application is the container that *waits*, while the init
///    container and the network sidecar wait on nothing. The earlier heuristic
///    preferred a container *without* dependsOn unconditionally, which is
///    backwards for that shape and resolved to the sidecar.
/// 3. Essential container with log configuration
/// 4. Any essential container
/// 5. With nothing essential to go on, a container without `dependsOn` — at
///    that point the init-container signal is the reliable one and points the
///    other way.
/// 6. First non-infra container
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

    // The application is the container that waits for the others. Restrict to
    // essential containers so a non-essential helper that happens to declare a
    // dependency does not win.
    let dependent_essential = app_containers
        .iter()
        .find(|cd| !cd.depends_on().is_empty() && cd.essential().unwrap_or(false));
    if let Some(cd) = dependent_essential {
        return Some(cd);
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

    // No essential container to go on. Here the init-container signal is the
    // reliable one and points the other way: with nothing essential to
    // distinguish them, a container that waits on another is more likely to be
    // the init/helper and the one that waits on nothing is the app.
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

    /// The exact shape of a real deployed task: a non-essential init container
    /// that chowns a shared volume, a tailscale sidecar that makes the task
    /// reachable, and the application waiting on the init container. Before the
    /// dependsOn-first rule, exec resolved to the tailscale sidecar -- it is
    /// also essential, also has a log configuration, and came first in the
    /// list -- so `ecsctl exec` ran commands in the wrong container.
    #[test]
    fn picks_the_waiting_app_over_an_init_container_and_a_network_sidecar() {
        let containers = vec![
            make_container("init-perms", false, true, false),
            make_container("tailscale", true, true, false),
            make_container("openab-pty", true, true, true),
        ];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("openab-pty"));
    }

    #[test]
    fn a_nonessential_helper_with_a_dependency_does_not_win() {
        // dependsOn alone is not enough: the app must also be essential, or a
        // non-essential post-start helper would be picked over it.
        let containers = vec![
            make_container("post-start-hook", false, true, true),
            make_container("app", true, true, false),
        ];
        let main = find_main_container(&containers).unwrap();
        assert_eq!(main.name(), Some("app"));
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

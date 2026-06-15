use anyhow::{Context, Result};
use aws_sdk_cloudwatchlogs::Client as LogsClient;
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;

pub async fn run(
    config: &aws_config::SdkConfig,
    name: &str,
    lines: i32,
    follow: bool,
) -> Result<()> {
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

    // Find newest running task
    let tasks_resp = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(service)
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Running)
        .send()
        .await
        .context("ListTasks failed")?;

    let task_arns = tasks_resp.task_arns();
    if task_arns.is_empty() {
        anyhow::bail!("no RUNNING tasks for '{name}'");
    }

    let desc = ecs
        .describe_tasks()
        .cluster(cluster)
        .set_tasks(Some(task_arns.to_vec()))
        .send()
        .await?;

    let task = desc
        .tasks()
        .iter()
        .filter(|t| t.last_status() == Some("RUNNING"))
        .max_by_key(|t| t.started_at())
        .context("no RUNNING tasks")?;

    let task_id = task
        .task_arn()
        .unwrap_or("?")
        .rsplit('/')
        .next()
        .unwrap_or("?");
    let task_def_arn = task.task_definition_arn().context("no task def ARN")?;

    // Get log config from task definition
    let td = ecs
        .describe_task_definition()
        .task_definition(task_def_arn)
        .send()
        .await?
        .task_definition
        .context("no task definition")?;

    let cd = td
        .container_definitions()
        .iter()
        .find(|cd| {
            !cd.name()
                .unwrap_or_default()
                .starts_with("ecs-service-connect-")
        })
        .context("no app container found")?;

    let log_config = cd.log_configuration().context("no log configuration")?;
    let opts = log_config.options().context("no log options")?;
    let group = opts.get("awslogs-group").context("no awslogs-group")?;
    let prefix = opts
        .get("awslogs-stream-prefix")
        .context("no awslogs-stream-prefix")?;
    let container_name = cd.name().unwrap_or("app");
    let stream_name = format!("{prefix}/{container_name}/{task_id}");

    let logs = LogsClient::new(config);

    if follow {
        tail_follow(&logs, group, &stream_name, lines).await
    } else {
        let resp = logs
            .get_log_events()
            .log_group_name(group)
            .log_stream_name(&stream_name)
            .limit(lines)
            .start_from_head(false)
            .send()
            .await
            .context("GetLogEvents failed")?;

        for event in resp.events() {
            let msg = event.message().unwrap_or("");
            println!("{msg}");
        }
        Ok(())
    }
}

async fn tail_follow(
    logs: &LogsClient,
    group: &str,
    stream: &str,
    initial_lines: i32,
) -> Result<()> {
    // Get initial batch
    let resp = logs
        .get_log_events()
        .log_group_name(group)
        .log_stream_name(stream)
        .limit(initial_lines)
        .start_from_head(false)
        .send()
        .await
        .context("GetLogEvents failed")?;

    for event in resp.events() {
        let msg = event.message().unwrap_or("");
        println!("{msg}");
    }

    // Use the forward token to poll for new events
    let mut next_token = resp.next_forward_token().map(|s| s.to_string());

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let mut req = logs
            .get_log_events()
            .log_group_name(group)
            .log_stream_name(stream)
            .start_from_head(true);

        if let Some(ref token) = next_token {
            req = req.next_token(token);
        }

        let resp = req.send().await.context("GetLogEvents failed")?;

        for event in resp.events() {
            let msg = event.message().unwrap_or("");
            println!("{msg}");
        }

        next_token = resp.next_forward_token().map(|s| s.to_string());
    }
}

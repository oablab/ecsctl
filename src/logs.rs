use anyhow::{Context, Result};
use aws_sdk_cloudwatchlogs::Client as LogsClient;
use aws_sdk_ecs::Client as EcsClient;

use crate::config::Config;
use crate::container::find_main_container;

pub async fn run(
    config: &aws_config::SdkConfig,
    cfg: &Config,
    name: &str,
    lines: i32,
    follow: bool,
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

    // Find newest task (running or stopped)
    let mut task_arns: Vec<String> = Vec::new();

    let running_resp = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(service)
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Running)
        .send()
        .await
        .context("ListTasks failed")?;
    task_arns.extend(running_resp.task_arns().iter().cloned());

    if task_arns.is_empty() {
        // No running tasks — try stopped tasks for crash logs
        let stopped_resp = ecs
            .list_tasks()
            .cluster(cluster)
            .service_name(service)
            .desired_status(aws_sdk_ecs::types::DesiredStatus::Stopped)
            .send()
            .await
            .context("ListTasks (stopped) failed")?;
        task_arns.extend(stopped_resp.task_arns().iter().cloned());
    }

    if task_arns.is_empty() {
        anyhow::bail!("no tasks found for '{name}' (running or recently stopped)");
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
        .max_by_key(|t| t.started_at())
        .context("no tasks found")?;

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

    let cd = find_main_container(td.container_definitions()).context("no app container found")?;

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
        return tail_follow(&logs, group, &stream_name, lines).await;
    }

    let events = get_events(&logs, group, &stream_name, lines).await?;
    if !events.is_empty() {
        for msg in &events {
            println!("{msg}");
        }
        return Ok(());
    }

    // The selected task's stream has no events yet (e.g. the task is PENDING
    // during a replacement). Fall back to the newest stream that has events
    // instead of silently printing nothing.
    let status = task.last_status().unwrap_or("UNKNOWN");
    eprintln!("ecsctl: no log events yet for task {task_id} ({status})");

    let stream_prefix = format!("{prefix}/{container_name}/");
    match fetch_fallback_events(
        &ecs,
        &logs,
        cluster,
        service,
        group,
        &stream_prefix,
        &stream_name,
        lines,
    )
    .await?
    {
        Some((stream, events)) => {
            let prev_task = stream.rsplit('/').next().unwrap_or("?");
            eprintln!("ecsctl: showing logs from previous task {prev_task}");
            for msg in &events {
                println!("{msg}");
            }
        }
        None => {
            eprintln!(
                "ecsctl: no log streams with events found in '{group}' under prefix '{stream_prefix}'"
            );
        }
    }
    Ok(())
}

/// Fetch up to `lines` log events from a stream, tail-first.
/// A missing stream (not created yet) is treated as empty, not an error;
/// a missing log group is a hard error (permanent misconfiguration).
///
/// GetLogEvents may return empty pages that are nonterminal, so an empty
/// page only proves emptiness once the backward token stops advancing
/// (bounded at `EMPTY_PAGE_LIMIT` requests).
const EMPTY_PAGE_LIMIT: usize = 3;

async fn get_events(
    logs: &LogsClient,
    group: &str,
    stream: &str,
    lines: i32,
) -> Result<Vec<String>> {
    let mut token: Option<String> = None;
    for _ in 0..EMPTY_PAGE_LIMIT {
        let mut req = logs
            .get_log_events()
            .log_group_name(group)
            .log_stream_name(stream)
            .limit(lines)
            .start_from_head(false);
        if let Some(ref t) = token {
            req = req.next_token(t);
        }
        match req.send().await {
            Ok(out) => {
                if !out.events().is_empty() {
                    return Ok(out
                        .events()
                        .iter()
                        .map(|e| e.message().unwrap_or("").to_string())
                        .collect());
                }
                // The start of the stream is reached when the returned token
                // equals the one we passed in; otherwise keep paging backward.
                let next = out.next_backward_token().map(|s| s.to_string());
                if next.is_none() || next == token {
                    return Ok(Vec::new());
                }
                token = next;
            }
            Err(e) => match classify_not_found(&e) {
                Some(NotFound::Stream) => return Ok(Vec::new()),
                Some(NotFound::Group) => {
                    return Err(e).context(format!("log group '{group}' does not exist"));
                }
                None => return Err(e).context("GetLogEvents failed"),
            },
        }
    }
    Ok(Vec::new())
}

enum NotFound {
    Stream,
    Group,
}

/// Distinguish a missing log stream (transient — the task has not started
/// logging yet) from a missing log group (permanent misconfiguration).
fn classify_not_found(
    err: &aws_sdk_cloudwatchlogs::error::SdkError<
        aws_sdk_cloudwatchlogs::operation::get_log_events::GetLogEventsError,
    >,
) -> Option<NotFound> {
    use aws_sdk_cloudwatchlogs::error::ProvideErrorMetadata;
    let se = err.as_service_error()?;
    if !se.is_resource_not_found_exception() {
        return None;
    }
    let msg = se.message().unwrap_or("").to_ascii_lowercase();
    if msg.contains("log group") {
        Some(NotFound::Group)
    } else {
        Some(NotFound::Stream)
    }
}

/// Find the newest stream with events, excluding `exclude`.
///
/// Strategy 1: recently stopped tasks of the service (ECS retains them ~1h),
/// newest first — this covers the common task-replacement window precisely.
/// Only the `STOPPED_TASK_PROBE_LIMIT` newest stopped tasks are probed to
/// bound request fan-out.
/// Strategy 2: best-effort scan of log streams under the prefix, picking the
/// stream with the latest lastEventTimestamp. (DescribeLogStreams cannot
/// combine a name prefix with LastEventTime ordering, so page by name and
/// take the max client-side, capped at a few pages.)
const STOPPED_TASK_PROBE_LIMIT: usize = 5;

#[allow(clippy::too_many_arguments)]
async fn fetch_fallback_events(
    ecs: &EcsClient,
    logs: &LogsClient,
    cluster: &str,
    service: &str,
    group: &str,
    stream_prefix: &str,
    exclude: &str,
    lines: i32,
) -> Result<Option<(String, Vec<String>)>> {
    // Strategy 1: recently stopped tasks, newest first
    let stopped_resp = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(service)
        .desired_status(aws_sdk_ecs::types::DesiredStatus::Stopped)
        .send()
        .await
        .context("ListTasks (stopped) failed")?;
    let stopped_arns: Vec<String> = stopped_resp.task_arns().to_vec();

    if !stopped_arns.is_empty() {
        let desc = ecs
            .describe_tasks()
            .cluster(cluster)
            .set_tasks(Some(stopped_arns))
            .send()
            .await?;
        let mut tasks = desc.tasks().to_vec();
        tasks.sort_by_key(|t| std::cmp::Reverse(t.stopped_at().or(t.started_at()).copied()));

        for t in tasks.iter().take(STOPPED_TASK_PROBE_LIMIT) {
            let tid = t
                .task_arn()
                .unwrap_or("?")
                .rsplit('/')
                .next()
                .unwrap_or("?");
            let stream = format!("{stream_prefix}{tid}");
            if stream == exclude {
                continue;
            }
            let events = get_events(logs, group, &stream, lines).await?;
            if !events.is_empty() {
                return Ok(Some((stream, events)));
            }
        }
    }

    // Strategy 2: scan streams under the prefix for the latest lastEventTimestamp
    let mut best: Option<(i64, String)> = None;
    let mut next_token: Option<String> = None;
    for _ in 0..4 {
        let mut req = logs
            .describe_log_streams()
            .log_group_name(group)
            .log_stream_name_prefix(stream_prefix)
            .limit(50);
        if let Some(t) = next_token {
            req = req.next_token(t);
        }
        let resp = req.send().await.context("DescribeLogStreams failed")?;
        for s in resp.log_streams() {
            let (Some(name), Some(ts)) = (s.log_stream_name(), s.last_event_timestamp()) else {
                continue;
            };
            if name == exclude {
                continue;
            }
            if best.as_ref().map(|(bts, _)| ts > *bts).unwrap_or(true) {
                best = Some((ts, name.to_string()));
            }
        }
        next_token = resp.next_token().map(|s| s.to_string());
        if next_token.is_none() {
            break;
        }
    }

    if let Some((_, stream)) = best {
        let events = get_events(logs, group, &stream, lines).await?;
        if !events.is_empty() {
            return Ok(Some((stream, events)));
        }
    }

    Ok(None)
}

async fn tail_follow(
    logs: &LogsClient,
    group: &str,
    stream: &str,
    initial_lines: i32,
) -> Result<()> {
    // Get initial batch
    let mut next_token: Option<String> = None;

    let resp = logs
        .get_log_events()
        .log_group_name(group)
        .log_stream_name(stream)
        .limit(initial_lines)
        .start_from_head(false)
        .send()
        .await;

    match resp {
        Ok(resp) => {
            if resp.events().is_empty() {
                eprintln!(
                    "ecsctl: no log events yet in stream '{stream}' — waiting for new events..."
                );
            }
            for event in resp.events() {
                let msg = event.message().unwrap_or("");
                println!("{msg}");
            }
            next_token = resp.next_forward_token().map(|s| s.to_string());
        }
        Err(e) => match classify_not_found(&e) {
            Some(NotFound::Stream) => {
                eprintln!(
                    "ecsctl: log stream '{stream}' does not exist yet — waiting for the task to start logging..."
                );
            }
            Some(NotFound::Group) => {
                return Err(e).context(format!("log group '{group}' does not exist"));
            }
            None => return Err(e).context("GetLogEvents failed"),
        },
    }

    // Use the forward token to poll for new events
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

        match req.send().await {
            Ok(resp) => {
                for event in resp.events() {
                    let msg = event.message().unwrap_or("");
                    println!("{msg}");
                }
                next_token = resp.next_forward_token().map(|s| s.to_string());
            }
            Err(e) => match classify_not_found(&e) {
                // Stream still not created — keep waiting
                Some(NotFound::Stream) => continue,
                Some(NotFound::Group) => {
                    return Err(e).context(format!("log group '{group}' does not exist"));
                }
                None => return Err(e).context("GetLogEvents failed"),
            },
        }
    }
}

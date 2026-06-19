use clap::{Parser, Subcommand};
use ecsctl::{alias, apply, clone, cp, delete, exec, export, logs, restart, scale, sync, update};
use ecsctl::config::Config;

#[derive(Parser)]
#[command(
    name = "ecsctl",
    version,
    about = "kubectl-style CLI for AWS ECS Fargate"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy files to/from an ECS Fargate container via S3 presigned URLs
    Cp {
        /// Source (local path or alias:/path or cluster/task/container:/path)
        src: String,
        /// Destination (local path or alias:/path or cluster/task/container:/path)
        dst: String,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        presign_expiry: Option<u64>,
    },
    /// Sync a directory between local and container (tar + S3)
    Sync {
        /// Source: local dir or alias:/path
        src: String,
        /// Destination: alias:/path or local dir
        dst: String,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        presign_expiry: Option<u64>,
    },
    /// Execute a command in an ECS Fargate container
    Exec {
        /// alias or cluster/task/container
        target: String,
        /// Command to run (default: /bin/sh)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Apply a declarative service spec
    Apply {
        /// Path to YAML spec file
        #[arg(short = 'f', long = "file")]
        file: String,
        /// Override spec fields (e.g. --set spec.cpu=512 --set metadata.name=foo)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
        /// Wait for deployment to stabilize
        #[arg(long)]
        wait: bool,
    },
    /// Delete a service
    Delete {
        /// Alias name or service name
        name: Option<String>,
        /// Path to YAML spec file
        #[arg(short = 'f', long = "file")]
        file: Option<String>,
    },
    /// Force restart a service (new deployment)
    Restart {
        /// Alias name
        name: String,
        /// Wait for deployment to stabilize
        #[arg(long)]
        wait: bool,
    },
    /// Scale a service to a desired task count
    Scale {
        /// Alias name
        name: String,
        /// Desired task count (0 to N)
        count: i32,
        /// Wait for deployment to stabilize
        #[arg(long)]
        wait: bool,
    },
    /// Update a service in-place (export + apply --set without intermediate file)
    Update {
        /// Alias name
        name: String,
        /// Override spec fields (e.g. --set spec.cpu=512 --set spec.image=nginx:latest)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
        /// Wait for deployment to stabilize
        #[arg(long)]
        wait: bool,
    },
    /// Clone a service: export source → apply as new name
    Clone {
        /// Source alias
        source: String,
        /// New service name
        target: String,
        /// Override spec fields (e.g. --set spec.cpu=512)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
    },
    /// Export a running service to a YAML spec file
    Export {
        /// Alias name
        name: String,
        /// Output file (default: stdout)
        #[arg(short = 'f', long = "file")]
        output: Option<String>,
    },
    /// Manage aliases for cluster/service/container targets
    Alias {
        #[command(subcommand)]
        action: AliasAction,
    },
    /// Describe the resolved task for an alias
    Get {
        /// Alias name
        name: String,
        /// Output format: json, jsonpath='<template>'
        #[arg(short = 'o', long = "output")]
        output: Option<String>,
    },
    /// Show recent logs for an alias
    Log {
        /// Alias name
        name: String,
        /// Number of lines (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        lines: i32,
        /// Follow (live tail)
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum AliasAction {
    /// Set an alias: ecsctl alias set cluster/service/container name
    Set {
        /// Target: cluster/service/container[/task_id]
        target: String,
        /// Alias name
        name: String,
    },
    /// Remove an alias
    Rm { name: String },
    /// List all aliases
    Ls,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    match cli.command {
        Command::Apply {
            file,
            overrides,
            wait,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            apply::run(&aws_config, &file, &overrides, wait).await
        }
        Command::Delete { name, file } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            delete::run(&aws_config, name.as_deref(), file.as_deref()).await
        }
        Command::Restart { name, wait } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            restart::run(&aws_config, &name, wait).await
        }
        Command::Scale { name, count, wait } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            scale::run(&aws_config, &name, count, wait).await
        }
        Command::Update {
            name,
            overrides,
            wait,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            update::run(&aws_config, &name, &overrides, wait).await
        }
        Command::Clone {
            source,
            target,
            overrides,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            clone::run(&aws_config, &source, &target, &overrides).await
        }
        Command::Export { name, output } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            export::run(&aws_config, &name, output.as_deref()).await
        }
        Command::Alias { action } => match action {
            AliasAction::Set { target, name } => alias::set(&name, &target).await,
            AliasAction::Rm { name } => alias::remove(&name).await,
            AliasAction::Ls => alias::list().await,
        },
        Command::Get { name, output } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            alias::describe(&aws_config, &name, output.as_deref()).await
        }
        Command::Log {
            name,
            lines,
            follow,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            logs::run(&aws_config, &name, lines, follow).await
        }
        Command::Exec { target, command } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let resolved = alias::resolve(&aws_config, &target).await?;
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command.join(" "))
            };
            exec::run(&aws_config, &resolved, cmd.as_deref()).await
        }
        Command::Cp {
            src,
            dst,
            bucket,
            presign_expiry,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let expiry = presign_expiry.unwrap_or(cfg.presign_expiry_secs());
            let bucket = bucket.or(cfg.bucket);
            // Resolve aliases in remote paths (the one with ':')
            let src = resolve_remote_alias(&aws_config, &src).await?;
            let dst = resolve_remote_alias(&aws_config, &dst).await?;
            cp::run(&aws_config, &src, &dst, bucket.as_deref(), expiry).await
        }
        Command::Sync {
            src,
            dst,
            bucket,
            presign_expiry,
        } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let expiry = presign_expiry.unwrap_or(cfg.presign_expiry_secs());
            let bucket = bucket.or(cfg.bucket);
            let src = resolve_remote_alias(&aws_config, &src).await?;
            let dst = resolve_remote_alias(&aws_config, &dst).await?;
            let src_remote = src.contains(':') && !src.starts_with('/');
            let dst_remote = dst.contains(':') && !dst.starts_with('/');
            match (src_remote, dst_remote) {
                (false, true) => {
                    sync::run(&aws_config, &src, &dst, bucket.as_deref(), expiry).await
                }
                (true, false) => {
                    sync::run_download(&aws_config, &src, &dst, bucket.as_deref(), expiry).await
                }
                _ => anyhow::bail!("exactly one of src/dst must be a remote path (alias:/path)"),
            }
        }
    }
}

/// If a string is "alias:/path", resolve the alias part to cluster/task/container:/path
async fn resolve_remote_alias(config: &aws_config::SdkConfig, s: &str) -> anyhow::Result<String> {
    if let Some(colon_pos) = s.find(':') {
        let prefix = &s[..colon_pos];
        let path = &s[colon_pos..]; // includes the ':'
                                    // If prefix doesn't contain '/' it might be an alias
        if !prefix.contains('/') {
            let resolved = alias::resolve(config, prefix).await?;
            if resolved != prefix {
                return Ok(format!("{resolved}{path}"));
            }
        }
    }
    Ok(s.to_string())
}

mod alias;
mod apply;
mod config;
mod cp;
mod delete;
mod exec;
mod logs;
mod restart;
mod sync;

use clap::{Parser, Subcommand};
use config::Config;

#[derive(Parser)]
#[command(name = "ecsctl", about = "kubectl-style CLI for AWS ECS Fargate")]
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
    /// Sync a local directory to a container (tar + upload + extract)
    Sync {
        /// Local directory path
        src: String,
        /// Remote target: alias:/path or cluster/task/container:/path
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
        /// Output as JSON (pipe to jq for field selection)
        #[arg(long)]
        json: bool,
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
        Command::Apply { file } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            apply::run(&aws_config, &file).await
        }
        Command::Delete { name, file } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            delete::run(&aws_config, name.as_deref(), file.as_deref()).await
        }
        Command::Restart { name } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            restart::run(&aws_config, &name).await
        }
        Command::Alias { action } => match action {
            AliasAction::Set { target, name } => alias::set(&name, &target).await,
            AliasAction::Rm { name } => alias::remove(&name).await,
            AliasAction::Ls => alias::list().await,
        },
        Command::Get { name, json } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            alias::describe(&aws_config, &name, json).await
        }
        Command::Log { name, lines, follow } => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            logs::run(&aws_config, &name, lines, follow).await
        }
        Command::Exec {
            target,
            command,
        } => {
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
            let dst = resolve_remote_alias(&aws_config, &dst).await?;
            sync::run(&aws_config, &src, &dst, bucket.as_deref(), expiry).await
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

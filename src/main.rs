mod config;
mod cp;
mod exec;

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
        /// Source (local path or cluster/task/container:/remote/path)
        src: String,
        /// Destination (local path or cluster/task/container:/remote/path)
        dst: String,
        /// S3 bucket for staging (overrides config)
        #[arg(long)]
        bucket: Option<String>,
        /// Presigned URL expiry in seconds (overrides config)
        #[arg(long)]
        presign_expiry: Option<u64>,
    },
    /// Execute a command in an ECS Fargate container
    Exec {
        /// cluster/task/container
        target: String,
        /// Command to run
        #[arg(long, short)]
        command: Option<String>,
        /// Interactive mode
        #[arg(long, short)]
        interactive: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    match cli.command {
        Command::Cp {
            src,
            dst,
            bucket,
            presign_expiry,
        } => {
            let expiry = presign_expiry.unwrap_or(cfg.presign_expiry_secs());
            let bucket = bucket.or(cfg.bucket);
            cp::run(&aws_config, &src, &dst, bucket.as_deref(), expiry).await
        }
        Command::Exec {
            target,
            command,
            interactive,
        } => exec::run(&aws_config, &target, command.as_deref(), interactive).await,
    }
}

mod cp;
mod exec;

use clap::{Parser, Subcommand};

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
        /// S3 bucket for staging (default: ecsctl-staging-{account_id})
        #[arg(long)]
        bucket: Option<String>,
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
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    match cli.command {
        Command::Cp { src, dst, bucket } => cp::run(&config, &src, &dst, bucket.as_deref()).await,
        Command::Exec {
            target,
            command,
            interactive,
        } => exec::run(&config, &target, command.as_deref(), interactive).await,
    }
}

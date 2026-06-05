# ecsctl

kubectl-style CLI for AWS ECS Fargate.

## Commands

### `ecsctl exec` — interactive shell into a container

```bash
ecsctl exec my-cluster/TASK_ID/my-container -i
ecsctl exec my-cluster/TASK_ID/my-container -c "cat /etc/os-release"
```

### `ecsctl cp` — copy files to/from a container

```bash
# Upload local file to container
ecsctl cp myfile.txt my-cluster/TASK_ID/my-container:/tmp/myfile.txt

# Download file from container
ecsctl cp my-cluster/TASK_ID/my-container:/tmp/output.log ./output.log
```

### `ecsctl sync` — sync a local directory to a container

```bash
ecsctl sync ./my-app my-cluster/TASK_ID/my-container:/opt/app
```

Behind the scenes:
```
./my-app/ → tar czf → S3 upload → presigned GET URL → ECS Exec: wget/curl | tar xzf -C /opt/app → S3 cleanup
```

## How it works

```
┌──────────┐       ┌────────┐       ┌─────────────────────────────┐
│  Local   │──tar──▶│   S3   │       │  ECS Fargate Container      │
│  Machine │       │ Bucket │       │                             │
│          │       │        │──presigned URL──▶ wget/curl │ tar x │
│          │       │(delete)│◀──────│                             │
└──────────┘       └────────┘       └─────────────────────────────┘
```

No AWS CLI needed inside the container — only `curl`/`wget` (+ `tar` for sync).

## Configuration

`~/.ecsctl/config.toml`:

```toml
# S3 bucket for staging (auto-created as ecsctl-staging-{account_id} if unset)
# bucket = "my-custom-bucket"

# Presigned URL expiry in seconds (default: 60)
presign_expiry = 60

# Default cluster name
# cluster = "my-cluster"
```

Priority: CLI flags > config.toml > defaults.

## Requirements

- AWS credentials configured
- ECS Exec enabled on the service (`EnableExecuteCommand: true`)
- Task role with SSM permissions
- Container must have `curl` or `wget` (+ `tar` for sync)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) for interactive exec

## Install

```bash
cargo install --path .
```

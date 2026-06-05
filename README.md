# ecsctl

kubectl-style CLI for AWS ECS Fargate.

## Commands

### `ecsctl exec` — execute a command in a container

```bash
ecsctl exec chaodu                       # /bin/sh (default)
ecsctl exec chaodu bash                  # bash
ecsctl exec chaodu -- cat /etc/hosts     # command with args
ecsctl exec chaodu echo hello world      # also works
```

### `ecsctl cp` — copy files to/from a container

```bash
ecsctl cp myfile.txt chaodu:/tmp/myfile.txt      # upload
ecsctl cp chaodu:/tmp/output.log ./output.log    # download
```

### `ecsctl sync` — sync a local directory to a container

```bash
ecsctl sync ./my-app chaodu:/opt/app
```

### `ecsctl alias` — manage target aliases

```bash
ecsctl alias set my-cluster/my-service myapp         # auto-resolve task + container
ecsctl alias set my-cluster/my-service/app myapp     # auto-resolve task only
ecsctl alias set my-cluster/my-service/app/ID myapp  # fully pinned
ecsctl alias ls                                       # list all
ecsctl alias rm myapp                                 # remove
```

Alias format: `cluster/service[/container[/task_id]]`

| Parts | Example | Behavior |
|-------|---------|----------|
| 2 | `openab/openab-chaodu` | Auto-resolve newest RUNNING task + container name |
| 3 | `openab/openab-chaodu/app` | Auto-resolve newest RUNNING task |
| 4 | `openab/openab-chaodu/app/abc123` | Fully pinned |

## How `cp` and `sync` work

```
┌──────────┐       ┌────────┐       ┌─────────────────────────────┐
│  Local   │──tar──▶│   S3   │       │  ECS Fargate Container      │
│  Machine │       │ Bucket │       │                             │
│          │       │        │──presigned URL──▶ wget/curl | tar x │
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

[aliases]
myapp = "my-cluster/my-service"
```

Priority: CLI flags > config.toml > defaults.

## Requirements

- AWS credentials configured
- ECS Exec enabled on the service (`EnableExecuteCommand: true`)
- Task role with SSM permissions
- Container must have `curl` or `wget` (+ `tar` for sync)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) installed locally

## Shell Aliases

```bash
# Add to ~/.bashrc or ~/.zshrc
ecsh() { ecsctl exec "$1" bash; }

# Usage
ecsh chaodu       # bash into chaodu's newest running task
```

## Install

```bash
cargo install --path .
```

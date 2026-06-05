# ecsctl

A wrapper around ECS Exec that gives you a kubectl-like experience on Amazon ECS.

```bash
ecsctl exec chaodu bash          # like: kubectl exec -it pod -- bash
ecsctl cp file.txt chaodu:/tmp/  # like: kubectl cp file.txt pod:/tmp/
ecsctl sync ./app chaodu:/opt/   # tar + upload + extract
ecsctl get chaodu                # like: kubectl describe pod
ecsctl log chaodu -f             # like: kubectl logs -f pod
```

## Commands

### `ecsctl exec` — execute a command in a container

```bash
ecsctl exec chaodu                       # /bin/sh (default)
ecsctl exec chaodu bash                  # bash
ecsctl exec chaodu -- cat /etc/hosts     # command with args
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

### `ecsctl get` — describe a task

```bash
ecsctl get chaodu              # human-readable
ecsctl get chaodu --json       # JSON (pipe to jq)
ecsctl get chaodu --json | jq '.tasks[0].capacity'     # "FARGATE_SPOT"
ecsctl get chaodu --json | jq '.tasks[0].containers[1].env'
```

Output includes: status, health, CPU/memory, arch (X86_64/ARM64), capacity provider (FARGATE/FARGATE_SPOT), AZ, connectivity, exec status, env vars (secrets masked), and last 10 log lines.

### `ecsctl log` — view logs

```bash
ecsctl log chaodu              # last 20 lines
ecsctl log chaodu -n 50        # last 50 lines
ecsctl log chaodu -f           # live tail (Ctrl+C to stop)
ecsctl log chaodu -f -n 10     # start with last 10, then follow
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

## Shell Aliases

```bash
# Add to ~/.bashrc or ~/.zshrc
ecsh() { ecsctl exec "$1" bash; }

# Usage
ecsh chaodu       # bash into chaodu's newest running task
```

## Requirements

- AWS credentials configured
- ECS Exec enabled on the service (`EnableExecuteCommand: true`)
- Task role with SSM permissions
- Container must have `curl` or `wget` (+ `tar` for sync)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) installed locally

## Install

```bash
cargo install --path .
```

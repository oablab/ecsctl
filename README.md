# ecsctl

An agent-first CLI that gives you a kubectl-like experience on Amazon ECS. Built for deploying and managing AI agent workloads — OpenClaw, Hermes, OpenAB, and any containerized agent — as easily as generic ECS services.

## Features

- **Agent-ready** — deploy AI agent frameworks (OpenClaw, Hermes, OpenAB, etc.) with a single YAML spec
- **Declarative deployments** — `apply` / `delete` / `restart` / `export` services with a simple YAML spec
- **Interactive shell** — `exec` into running containers instantly
- **File transfer** — `cp` files to/from containers via S3 presigned URLs (no AWS CLI needed in container)
- **Directory sync** — `sync` local directories into containers (tar + upload + extract)
- **Observability** — `get` task details and `log` with live tail
- **Alias system** — short names for cluster/service/container targets
- **Round-trip workflow** — `export` → edit → `apply`
- **Clone** — `clone` a running service under a new name with optional overrides
- **Scale** — `scale` a service to any desired task count (0 to N)
- **Sugar shell aliases** — `ecsh`, `ecscp`, `ecsync` for quick one-liners

## Quick Start

```bash
ecsctl apply -f service.yaml    # like: kubectl apply -f
ecsctl exec chaodu bash          # like: kubectl exec -it pod -- bash
ecsctl cp file.txt chaodu:/tmp/  # like: kubectl cp
ecsctl sync ./app chaodu:/opt/   # tar + upload + extract
ecsctl get chaodu                # like: kubectl describe pod
ecsctl log chaodu -f             # like: kubectl logs -f
ecsctl clone chaodu chaodu2      # clone with a new name
ecsctl delete chaodu             # like: kubectl delete
```

## Commands

| Command | Description |
|---------|-------------|
| `ecsctl apply -f <file>` | Deploy a service declaratively |
| `ecsctl delete <alias>` | Remove a service (scales to 0, deletes) |
| `ecsctl restart <alias>` | Force a rolling restart |
| `ecsctl scale <alias> <count>` | Scale a service to N desired tasks |
| `ecsctl clone <src> <dst>` | Clone a service under a new name |
| `ecsctl export <alias>` | Export a running service to YAML |
| `ecsctl exec <alias> [cmd]` | Execute a command in a container |
| `ecsctl cp <src> <dst>` | Copy files to/from a container |
| `ecsctl sync <dir> <alias>:<path>` | Sync a local directory to a container |
| `ecsctl get <alias>` | Describe a task (status, health, resources) |
| `ecsctl log <alias> [-f] [-n N]` | View or tail logs |
| `ecsctl alias set\|ls\|rm` | Manage target aliases |

### `ecsctl apply` — deploy a service declaratively

```bash
ecsctl apply -f service.yaml
ecsctl apply -f https://example.com/service.yaml   # remote URL
ecsctl apply -f service.yaml --set spec.cpu=1024 --set metadata.name=my-app2
ecsctl apply -f service.yaml --wait
```

Registers a task definition and creates/updates the ECS service. Auto-registers an alias.

- `-f` accepts a local file path or a remote HTTPS URL
- `--set KEY=VALUE` — override spec fields without editing the YAML (repeatable)
- `--wait` — block until the deployment stabilizes (all tasks running)

### `ecsctl delete` — remove a service

```bash
ecsctl delete chaodu              # by alias
ecsctl delete -f service.yaml     # by spec file
ecsctl delete -f https://example.com/service.yaml  # remote URL
```

Scales to 0, deletes the service, removes the alias.

### `ecsctl restart` — force restart a service

```bash
ecsctl restart chaodu
```

Triggers a new deployment (rolling replacement of all tasks).

### `ecsctl scale` — scale a service

```bash
ecsctl scale chaodu 0          # scale to 0 (no running tasks)
ecsctl scale chaodu 1          # scale to 1 task
ecsctl scale chaodu 3 --wait   # scale to 3 and wait for stabilization
```

Sets the desired task count for a service. Use `--wait` to block until all tasks are running.

### `ecsctl clone` — clone a service

```bash
ecsctl clone botA botB                              # exact copy, new name
ecsctl clone botA botB --set spec.cpu=2048          # clone with overrides
ecsctl clone botA botB --set spec.capacity=FARGATE  # switch to on-demand
```

Exports the source service and deploys it under a new name. Supports `--set` for overrides.

### `ecsctl export` — export a running service to YAML

```bash
ecsctl export chaodu                  # → service.yaml
ecsctl export chaodu -o chaodu.yaml   # custom output file
```

Enables round-trip workflows: export → edit → apply.

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
ecsctl get chaodu -o json      # JSON (pipe to jq)
ecsctl get chaodu -o json | jq '.tasks[0].capacity'
ecsctl get chaodu -o "jsonpath='http://{.tasks[0].public_ip}:8080'"  # template
```

Output includes: status, public/private IP, health, CPU/memory, arch, capacity provider, AZ, connectivity, exec status, env vars (secrets masked), and recent logs.

### `ecsctl log` — view logs

```bash
ecsctl log chaodu              # last 20 lines
ecsctl log chaodu -n 50        # last 50 lines
ecsctl log chaodu -f           # live tail (Ctrl+C to stop)
```

### `ecsctl alias` — manage target aliases

```bash
ecsctl alias set my-cluster/my-service myapp
ecsctl alias ls
ecsctl alias rm myapp
```

Alias format: `cluster/service[/container[/task_id]]`. Omitted parts are auto-resolved at runtime.

## Shell Aliases (Sugar)

Add to `~/.bashrc` or `~/.zshrc`:

```bash
ecsh()   { ecsctl exec "$1" bash; }
ecscp()  { ecsctl cp "$1" "$2"; }
ecsync() { ecsctl sync "$1" "$2"; }
```

| Alias | Equivalent | Example |
|-------|-----------|---------|
| `ecsh <alias>` | `ecsctl exec <alias> bash` | `ecsh chaodu` |
| `ecscp <src> <dst>` | `ecsctl cp <src> <dst>` | `ecscp myfile.txt chaodu:/tmp/` |
| `ecsync <dir> <dst>` | `ecsctl sync <dir> <dst>` | `ecsync ./app chaodu:/opt/app` |

## Service Spec

Add this comment for editor autocomplete (VS Code + YAML extension):

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/oablab/ecsctl/master/schemas/service.schema.json
apiVersion: ecsctl/v1
kind: Service
metadata:
  name: my-app
  cluster: my-cluster
spec:
  image: nginx:latest
  cpu: "256"
  memory: "512"
  arch: X86_64              # or ARM64
  capacity: FARGATE_SPOT    # or FARGATE
  desiredCount: 1
  execEnabled: true
  port: 80
  containerName: app
  executionRoleArn: arn:aws:iam::...:role/ecsTaskExecutionRole
  taskRoleArn: arn:aws:iam::...:role/my-task-role
  subnets: [subnet-aaa, subnet-bbb]
  securityGroups: [sg-xxx]
  assignPublicIp: false
  logGroup: /ecs/my-app
  env:
    APP_ENV: production
  secrets:
    DB_PASSWORD: arn:aws:secretsmanager:...:secret:my-db
  command: ["sh", "-c", "exec my-app serve"]   # optional: override container command
```

## How `cp` and `sync` work

```
┌──────────┐       ┌────────┐       ┌─────────────────────────────┐
│  Local   │──tar─▶│   S3   │       │  ECS Fargate Container      │
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

[aliases]
myapp = "my-cluster/my-service"
```

## Requirements

- AWS credentials configured
- ECS Exec enabled on the service (`EnableExecuteCommand: true`)
- Task role with SSM permissions
- Container must have `curl` or `wget` (+ `tar` for sync)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) installed locally

## Install

```bash
# macOS (Apple Silicon)
curl -sL https://github.com/oablab/ecsctl/releases/latest/download/ecsctl-darwin-arm64.tar.gz | tar xz -O > ~/.local/bin/ecsctl && chmod +x ~/.local/bin/ecsctl

# Linux (x86_64)
curl -sL https://github.com/oablab/ecsctl/releases/latest/download/ecsctl-linux-amd64.tar.gz | tar xz -O > ~/.local/bin/ecsctl && chmod +x ~/.local/bin/ecsctl

# Linux (ARM64)
curl -sL https://github.com/oablab/ecsctl/releases/latest/download/ecsctl-linux-arm64.tar.gz | tar xz -O > ~/.local/bin/ecsctl && chmod +x ~/.local/bin/ecsctl
```

Or build from source:

```bash
cargo install --path .
```

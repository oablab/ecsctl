# ecsctl

A wrapper around ECS Exec that gives you a kubectl-like experience on Amazon ECS.

```bash
ecsctl apply -f service.yaml    # like: kubectl apply -f
ecsctl exec chaodu bash          # like: kubectl exec -it pod -- bash
ecsctl cp file.txt chaodu:/tmp/  # like: kubectl cp
ecsctl sync ./app chaodu:/opt/   # tar + upload + extract
ecsctl get chaodu                # like: kubectl describe pod
ecsctl log chaodu -f             # like: kubectl logs -f
ecsctl delete chaodu             # like: kubectl delete
```

## Commands

### `ecsctl apply` — deploy a service declaratively

```bash
ecsctl apply -f service.yaml
```

Registers a task definition and creates/updates the ECS service. Auto-registers an alias.

### `ecsctl delete` — remove a service

```bash
ecsctl delete chaodu              # by alias
ecsctl delete -f service.yaml     # by spec file
```

Scales to 0, deletes the service, removes the alias.

### `ecsctl restart` — force restart a service

```bash
ecsctl restart chaodu
```

Triggers a new deployment (rolling replacement of all tasks).

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
ecsctl get chaodu --json | jq '.tasks[0].capacity'
```

Output includes: status, health, CPU/memory, arch, capacity provider, AZ, connectivity, exec status, env vars (secrets masked), and recent logs.

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

## Service Spec

```yaml
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
```

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

[aliases]
myapp = "my-cluster/my-service"
```

## Shell Aliases

```bash
# Add to ~/.bashrc or ~/.zshrc
ecsh() { ecsctl exec "$1" bash; }

# Usage
ecsh chaodu
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

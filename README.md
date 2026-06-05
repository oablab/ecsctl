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

## How `cp` works

```
local file → S3 (presigned PUT) → ECS Exec runs wget/curl inside container → done
container  → ECS Exec runs curl PUT to S3 → S3 (presigned GET) → local file
```

No AWS CLI needed inside the container — only `curl` or `wget`.

Uses a staging S3 bucket (`ecsctl-staging-{account_id}`, auto-created on first use).
Objects are deleted immediately after transfer.

## Requirements

- AWS credentials configured
- ECS Exec enabled on the service (`EnableExecuteCommand: true`)
- Task role with SSM permissions
- Container must have `curl` or `wget` (most images do)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html) for interactive exec

## Install

```bash
cargo install --path .
```

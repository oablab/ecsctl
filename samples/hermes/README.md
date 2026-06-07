# Deploy Hermes Agent on ECS Fargate with ecsctl

Run a [Hermes Agent](https://github.com/NousResearch/hermes-agent) gateway on ECS Fargate.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  ECS Fargate (FARGATE_SPOT)                                     │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  Container: hermes                                        │  │
│  │  Image: nousresearch/hermes-agent:latest                  │  │
│  │                                                           │  │
│  │  ┌─────────────────────┐    ┌──────────────────────────┐  │  │
│  │  │  Gateway + API      │    │  Dashboard               │  │  │
│  │  │  :8642              │    │  :9119                    │  │  │
│  │  │                     │    │                          │  │  │
│  │  │  • OpenAI-compat    │    │  • Web UI               │  │  │
│  │  │  • Telegram/Discord │    │  • Sessions/Memory      │  │  │
│  │  │  • s6-supervised    │    │  • Config               │  │  │
│  │  └─────────────────────┘    └──────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
│                          │                                      │
└──────────────────────────┼──────────────────────────────────────┘
                           │
              ┌────────────▼──────┐
              │  CloudWatch Logs  │
              │  /ecs/hermes      │
              └───────────────────┘
```

## Prerequisites

1. Create the CloudWatch log group:

```bash
aws logs create-log-group --log-group-name /ecs/hermes --region us-east-1
```

2. Update `minimal.yaml` with your account-specific values:
   - `ACCOUNT_ID` — your AWS account ID
   - `subnet-xxx` — a subnet with internet access
   - `sg-xxx` — security group allowing outbound + inbound 9119 (dashboard) and 8642 (API)

## Deploy

```bash
# From a local copy
ecsctl apply -f minimal.yaml --wait

# Or directly from the remote URL
ecsctl apply -f https://raw.githubusercontent.com/oablab/ecsctl/master/samples/hermes/minimal.yaml \
  --set metadata.cluster=my-cluster \
  --set spec.executionRoleArn=arn:aws:iam::ACCOUNT_ID:role/ecsTaskExecutionRole \
  --set spec.taskRoleArn=arn:aws:iam::ACCOUNT_ID:role/my-task-role \
  --wait
```

## Verify

```bash
ecsctl get hermes          # check task status
ecsctl log hermes -f       # tail logs — look for "Gateway Starting"
ecsctl exec hermes bash    # shell into the container

# Get the dashboard URL
ecsctl get hermes -o jsonpath='http://{.tasks[0].public_ip}:9119'

# Get the API URL
ecsctl get hermes -o jsonpath='http://{.tasks[0].public_ip}:8642'
```

## Onboard

After deployment, exec into the container to configure LLM providers:

```bash
ecsctl exec hermes bash

# Inside the container:
hermes setup
```

Or pass API keys via environment variables in the service spec:

```yaml
env:
  ANTHROPIC_API_KEY: sk-ant-...
  # or
  OPENAI_API_KEY: sk-...
```

## Notes

- **Image**: `nousresearch/hermes-agent:latest` — official from NousResearch (Docker Hub)
- **1 vCPU / 2 GB RAM** recommended minimum. Add `--shm-size=1g` equivalent if using browser tools.
- **Port 8642**: OpenAI-compatible API server (requires `API_SERVER_KEY`)
- **Port 9119**: Web dashboard (enable with `HERMES_DASHBOARD=1`)
- **s6-overlay**: PID 1 supervisor — auto-restarts gateway on crash
- **Data**: All state lives in `/opt/data`. Ephemeral on Fargate — use `ecscp`/`ecsync` for backup.
- **`HERMES_DASHBOARD_INSECURE=1`**: Skips auth gate — for demo only. Use OAuth or basic auth in production.

# Deploy OpenClaw on ECS Fargate with ecsctl

Run a minimal OpenClaw gateway on ECS Fargate — no LLM credentials required at startup.

## How it works

The official OpenClaw Docker image (`ghcr.io/openclaw/openclaw:latest`) requires a config file to skip the interactive setup wizard. We pre-seed a minimal `openclaw.json` at container startup with two key settings:

- `gateway.mode: "local"` — required or the gateway refuses to start
- `wizard.lastRunAt` — signals that setup has already completed

## Prerequisites

1. Create the CloudWatch log group (one-time):

```bash
aws logs create-log-group --log-group-name /ecs/openclaw --region us-east-1
```

2. Store a gateway token in Secrets Manager:

```bash
aws secretsmanager put-secret-value \
  --secret-id my-secret \
  --secret-string '{"OPENCLAW_GATEWAY_TOKEN":"'$(openssl rand -hex 32)'"}'
```

3. Update `minimal.yaml` with your account-specific values:
   - `ACCOUNT_ID` — your AWS account ID
   - `subnet-xxx` — a subnet with internet access
   - `sg-xxx` — a security group allowing outbound traffic (and inbound 18789 for web UI)

## Deploy

```bash
# From a local copy
ecsctl apply -f minimal.yaml

# Or directly from the remote URL
ecsctl apply -f https://raw.githubusercontent.com/oablab/ecsctl/master/samples/openclaw/minimal.yaml \
  --set metadata.cluster=my-cluster \
  --set spec.subnets[0]=subnet-xxx \
  --set spec.securityGroups[0]=sg-xxx \
  --set spec.executionRoleArn=arn:aws:iam::ACCOUNT_ID:role/ecsTaskExecutionRole \
  --set spec.taskRoleArn=arn:aws:iam::ACCOUNT_ID:role/my-task-role
```

## Onboard

After the gateway is running, exec into the container to complete onboarding:

```bash
ecsctl exec openclaw bash

# Inside the container — run the non-interactive onboard:
openclaw onboard --non-interactive \
  --mode local \
  --auth-choice apiKey \
  --anthropic-api-key "$ANTHROPIC_API_KEY" \
  --gateway-port 18789 \
  --gateway-bind lan \
  --skip-bootstrap \
  --skip-skills
```

Or configure a provider manually:

```bash
ecsctl exec openclaw bash

openclaw config set models.providers.anthropic.apiKey "sk-ant-..."
openclaw config set agents.defaults.model "anthropic/claude-sonnet-4-20250514"
```

You can also access the web UI at `http://<task-public-ip>:18789` and configure via Settings.

## Verify

```bash
ecsctl get openclaw        # check task status
ecsctl log openclaw -f     # tail logs — look for "[gateway] ready"
ecsctl exec openclaw bash  # shell into the container
```

## Notes

- **1 vCPU / 4 GB RAM** recommended. The image is ~1.5 GB and Node.js benefits from headroom.
- **Port 18789** is OpenClaw's default. The gateway exposes `/healthz` and `/readyz` for health checks.
- **FARGATE_SPOT** saves ~70% cost. Switch to `FARGATE` if you need guaranteed availability.
- **No LLM credentials at startup** — the gateway runs without any AI provider. Add them later.

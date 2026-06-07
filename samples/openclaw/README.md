# Deploy OpenClaw on ECS Fargate with ecsctl

Run a minimal OpenClaw gateway on ECS Fargate — no LLM credentials required at startup.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  ECS Fargate (FARGATE_SPOT)                                     │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  Container: openclaw                                      │  │
│  │  Image: ghcr.io/openclaw/openclaw:latest                  │  │
│  │                                                           │  │
│  │  ┌─────────────────────┐    ┌──────────────────────────┐  │  │
│  │  │  OpenClaw Gateway   │    │  /home/node/.openclaw/   │  │  │
│  │  │  :18789             │    │  ├── openclaw.json       │  │  │
│  │  │                     │    │  ├── agents/             │  │  │
│  │  │  • Web UI           │    │  └── workspace/          │  │  │
│  │  │  • REST API         │    │       (ephemeral)        │  │  │
│  │  │  • WebSocket        │    │                          │  │  │
│  │  └─────────────────────┘    └──────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
│                          │                     ▲                 │
└──────────────────────────┼─────────────────────┼─────────────────┘
                           │                     │
              ┌────────────▼──────┐    ┌─────────┴─────────┐
              │  CloudWatch Logs  │    │  Secrets Manager   │
              │  /ecs/openclaw    │    │  OPENCLAW_GATEWAY_ │
              └───────────────────┘    │  TOKEN             │
                                       └───────────────────┘

  ┌──────────┐         ecsync / ecscp          ┌──────────┐
  │  Local   │◀──────────────────────────────▶  │    S3    │
  │  Machine │   backup/restore .openclaw/      │  Bucket  │
  └──────────┘                                  └──────────┘
```

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

# Get the gateway URL
ecsctl get openclaw --json | jq -r '"http://\(.tasks[0].public_ip):18789"'
```

## Notes

- **1 vCPU / 4 GB RAM** recommended. The image is ~1.5 GB and Node.js benefits from headroom.
- **Port 18789** is OpenClaw's default. The gateway exposes `/healthz` and `/readyz` for health checks.
- **FARGATE_SPOT** saves ~70% cost. Switch to `FARGATE` if you need guaranteed availability.
- **No LLM credentials at startup** — the gateway runs without any AI provider. Add them later.
- **Origin check** — OpenClaw validates browser origins. The minimal config uses `dangerouslyAllowHostHeaderOriginFallback: true` to accept any origin matching the Host header. For production, set explicit `allowedOrigins` with your domain.

---

## Follow-up: Production Readiness

### 1. HTTPS behind ALB

The minimal setup exposes HTTP on a Fargate public IP — no TLS, IP changes on every deploy. For production, put it behind an ALB:

```
Browser → HTTPS :443 → ALB (ACM cert) → HTTP :18789 → Fargate task (private)
```

Benefits:
- **HTTPS termination** with an ACM certificate (free, auto-renewed)
- **Stable domain** — ALB DNS name or custom domain via Route 53
- **Fixed `allowedOrigins`** — no more IP guessing (`["https://openclaw.example.com"]`)
- **No public IP on task** — `assignPublicIp: false`, ALB routes to private subnet
- **Health checks** — ALB checks `/healthz` and replaces unhealthy tasks

> `ecsctl apply` will support ALB target group configuration soon. The service spec will accept a `targetGroup` field to wire up automatically.

### 2. Data Persistence

Fargate tasks are **ephemeral** — `/home/node/.openclaw/` (config, auth profiles, agent memory) is lost when the task is replaced. Use `ecscp` and `ecsync` to back up and restore state.

**Backup to local:**

```bash
ecscp openclaw:/home/node/.openclaw/ ./openclaw-backup/
```

**Restore after redeploy:**

```bash
ecsync ./openclaw-backup/ openclaw:/home/node/.openclaw/
ecsctl restart openclaw
```

**S3 as durable storage:**

```bash
# Backup: container → local → S3
ecscp openclaw:/home/node/.openclaw/ ./openclaw-backup/
aws s3 sync ./openclaw-backup/ s3://my-bucket/openclaw-config/

# Restore: S3 → local → container
aws s3 sync s3://my-bucket/openclaw-config/ ./openclaw-backup/
ecsync ./openclaw-backup/ openclaw:/home/node/.openclaw/
ecsctl restart openclaw
```

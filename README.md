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
- **Schedule** — `schedule` recurring scaling actions via EventBridge Scheduler (cron/rate)
- **In-place update** — `update` a service with `--set` overrides without export/apply
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
| `ecsctl schedule create\|list\|delete` | Manage recurring scaling schedules |
| `ecsctl update <alias> --set key=val` | Update a service in-place |
| `ecsctl clone <src> <dst>` | Clone a service under a new name |
| `ecsctl export <alias>` | Export a running service to YAML |
| `ecsctl exec <alias> [cmd]` | Execute a command in a container |
| `ecsctl cp <src> <dst>` | Copy files to/from a container |
| `ecsctl sync <dir> <alias>:<path>` | Sync a local directory to a container |
| `ecsctl get <alias>` | Describe a task (status, health, resources) |
| `ecsctl log <alias> [-f] [-n N]` | View or tail logs |
| `ecsctl alias set\|ls\|rm` | Manage target aliases |

> 📖 Full command reference, service spec, configuration, and requirements:
> **[docs/getting-started.md](docs/getting-started.md)**

## Highlights

### Declarative round-trip

```bash
ecsctl export chaodu -o bot.yaml   # snapshot a running service
vim bot.yaml                       # edit
ecsctl apply -f bot.yaml --wait    # redeploy and wait for stable
```

### Stop-then-start restarts for singleton services

```bash
ecsctl restart chaodu --recreate
```

The old task fully stops (shutdown hooks included) before the replacement
starts — no overlap window with duplicate bot tokens or stale state seeding.
Fails closed on unsupported service shapes and restores the deployment
configuration afterwards. Details and constraints:
[docs/getting-started.md](docs/getting-started.md#ecsctl-restart--force-restart-a-service-or-group).

### Fleet operations with groups

```bash
ecsctl scale @all 0                # stop the entire fleet
ecsctl restart @kiro --wait        # restart a named group
ecsctl schedule create chaodu 0 --expr 'cron(0 22 * * ? *)' --timezone 'Asia/Taipei'
```

### Into the container in one command

```bash
ecsctl exec chaodu bash            # interactive shell (ECS Exec)
ecsctl cp local.txt chaodu:/tmp/   # file transfer via S3 presigned URLs
ecsctl log chaodu -f               # live log tail
```

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

## Using as a Library

`ecsctl` can be consumed as a Rust library crate. Disable default features to avoid pulling CLI dependencies:

```toml
[dependencies]
ecsctl = { version = "0.11", default-features = false }
```

### Library-safe modules

The following modules are safe for library consumption with injected `&Config`:

- `ecsctl::config` — Config loading, alias/group resolution, scheduler config
- `ecsctl::scale` — Immediate scaling operations
- `ecsctl::restart` — Force new deployment
- `ecsctl::export` — Export service to YAML
- `ecsctl::logs` — CloudWatch log retrieval
- `ecsctl::scheduler` — Schedule creation/listing/deletion (⚠️ `#[doc(hidden)]`, API not yet stable — pending structured returns in follow-up)

### CLI-grade modules (not library-safe)

These modules contain alias persistence that writes to the default config path (`~/.ecsctl/config.toml`):

- `ecsctl::apply` — Creates services and auto-registers aliases
- `ecsctl::delete` — Deletes services and removes aliases
- `ecsctl::clone` — Clones via apply (inherits alias persistence)
- `ecsctl::update` — Updates via apply (inherits alias persistence)

### Custom config path

```rust
use ecsctl::config::Config;

let cfg = Config::load_from(Path::new("~/.oabctl/config.toml"))?;
```

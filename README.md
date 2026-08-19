# docker-dbake

Distributed Docker Bake — work-stealing across buildkitd nodes.

- [Why](#why)
- [How it works](#how-it-works)
- [Installation](#installation)
- [Usage](#usage)
  - [Options](#options)
  - [Examples](#examples)
- [TUI dashboard](#tui-dashboard)
  - [Log viewer](#log-viewer)
- [Scheduling](#scheduling)
  - [Work-stealing](#work-stealing)
  - [Platform awareness](#platform-awareness)
  - [Dependency handling](#dependency-handling)
  - [Failure handling](#failure-handling)
- [Shared cache with a registry](#shared-cache-with-a-registry)
  - [The problem](#the-problem)
  - [Using Zot as a shared cache registry](#using-zot-as-a-shared-cache-registry)
  - [How dbake integrates](#how-dbake-integrates)
  - [Cache flow](#cache-flow)
- [Shard lifecycle](#shard-lifecycle)
- [Builder setup](#builder-setup)
- [Project structure](#project-structure)
- [Development](#development)

## Why

Docker buildx has a multi-node builder concept. You can add multiple buildkitd instances to a single builder:

```bash
docker buildx create --name my-cluster --driver remote tcp://node1:1234
docker buildx create --name my-cluster --append --driver remote tcp://node2:1234
docker buildx create --name my-cluster --append --driver remote tcp://node3:1234
```

The expectation is that `docker buildx bake` would distribute targets across these nodes. **It doesn't.** Buildx sends every target through a single gRPC connection to one node. The other nodes sit idle. This is hardcoded in the buildx source — there's triple caching of the gRPC client that ensures one connection per build invocation.

Multi-node builders in buildx only serve one purpose: multi-platform builds, where an `arm64` layer goes to the ARM node and an `amd64` layer goes to the x86 node. But if you have 30 independent service images to build and 6 buildkitd instances, buildx will build all 30 sequentially on a single node.

There is no built-in way to:

- **Distribute independent targets across nodes.** Buildx has no target-level scheduling. Every target goes to the same node.
- **Work-steal across heterogeneous nodes.** If one node is fast (M3 Pro, 12 cores) and another is slow (Raspberry Pi, 4 cores), you want the fast node to pick up more work naturally. Buildx doesn't do this because it doesn't distribute work at all.
- **Target a specific node in a multi-node builder.** There's no `--node` flag. The only way to send a build to a specific node is to create a separate single-node builder pointing at it — which is exactly what `dbake` automates with ephemeral shard builders.

`dbake` fills this gap. It takes your existing multi-node builder, creates ephemeral single-node shards, and dispatches targets across them with platform-aware work-stealing.

## How it works

```
                        ┌─────────────┐
                        │  bake file  │
                        │ (compose/   │
                        │  HCL)       │
                        └──────┬──────┘
                               │
                    docker buildx bake --print
                               │
                        ┌──────▼──────┐
                        │   Target    │
                        │  Discovery  │
                        └──────┬──────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
      ┌───────▼──────┐ ┌──────▼───────┐ ┌──────▼───────┐
      │ shard-node0  │ │ shard-node1  │ │ shard-node2  │
      │ (ephemeral)  │ │ (ephemeral)  │ │ (ephemeral)  │
      └───────┬──────┘ └──────┬───────┘ └──────┬───────┘
              │                │                │
      ┌───────▼──────┐ ┌──────▼───────┐ ┌──────▼───────┐
      │  buildkitd   │ │  buildkitd   │ │  buildkitd   │
      │  tcp://.129  │ │  tcp://.144  │ │  tcp://.141  │
      └──────────────┘ └──────────────┘ └──────────────┘
```

1. Discovers all running TCP nodes in your buildx builder
2. Creates an ephemeral shard builder per node (`{builder}-shard-{node}--run{pid}`)
3. Resolves targets from `docker buildx bake --print` (works with both compose YAML and HCL)
4. Dispatches targets across nodes using work-stealing: each node builds one target at a time, then grabs the next available
5. Fast nodes naturally get more work — no need to predict build sizes

## Installation

```bash
# Build and install as Docker CLI plugin
make install

# Verify
docker dbake --help
```

Installs to `~/.docker/cli-plugins/docker-dbake`, making it available as `docker dbake`.

### Requirements

- Docker with buildx
- A multi-node buildx builder using the `remote` driver with TCP endpoints
- Rust toolchain (for building from source)

## Usage

```
docker dbake [OPTIONS] [TARGETS...]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-f, --file <PATH>` | `docker-compose.yml` | Compose or HCL bake file |
| `--builder <NAME>` | active builder | Buildx builder name |
| `--profile <NAME>` | — | Also build services gated behind this compose profile (repeatable) |
| `--with-deps` | — | Include `depends_on` chain for specified targets |
| `--exclude <A,B,C>` | — | Skip these targets (comma-separated) |
| `--platform <PLATFORM>` | — | Restrict shards *and* scheduling to these platforms (comma-separated) |
| `--cache-registry <URL>` | — | Registry for `type=registry` cache (`mode=max`) |
| `--no-cache` | — | Pass `--no-cache` to each bake invocation |
| `--load` | — | Load built images into local docker |
| `--push` | — | Push built images to registry |
| `--progress <MODE>` | `auto` | `auto` (TUI in terminal, plain otherwise) or `plain` |
| `--fail-fast` | — | Cancel all builds on first failure |

### Examples

```bash
# Build everything in docker-compose.yml across all nodes
docker dbake

# Build specific targets
docker dbake web api worker

# Build a target and its dependency chain
docker dbake web --with-deps

# Build only frontend profile, skip heavy targets
docker dbake --profile frontend --exclude elasticsearch

# Use a specific builder
docker dbake --builder my-cluster

# Build with shared registry cache (see below)
docker dbake --cache-registry registry.local:5000

# Push results, fail fast
docker dbake --push --fail-fast

# Use HCL bake file
docker dbake -f docker-bake.hcl
```

## TUI dashboard

When running in a terminal, `dbake` shows a live dashboard:

```
docker dbake — 3 nodes, 12 targets
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

 node0                    ████████░░ 4/5
   ✓ api                       12s
   ✓ web                        8s
 ▶ ⚙ worker              [building 23s...]
   ✓ cron                       3s

 node1                    ██████░░░░ 3/5
   ✓ redis                      2s
   ⚙ elasticsearch        [building 45s...]

 Queue: 2 pending [stream, mailer]
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 ✓ 8 done | ⚙ 2 building | ○ 2 pending | ✗ 0 failed | 1m 23s | j/k:select Enter:logs
```

**Keys:**

| Key | Action |
|-----|--------|
| `j` / `k` / arrows | Select target |
| `Enter` | View build log |
| `q` / `Esc` | Quit |
| `Ctrl+C` | Cancel all builds and quit |

In non-TTY environments (CI, pipes), falls back to line-by-line output.

### Log viewer

Press `Enter` on any target to view its build log in real time — no need to exit the TUI or wait for the build to finish. This is especially useful for diagnosing failures while other builds are still running.

```
 mysql [node2] FAILED: bake failed for target mysql (log: /tmp/dbake-logs/8123/mysql.log)
┌─────────────── Build Log ────────────────────────────────────────────────────┐
│ #8 [stage-1 2/5] RUN apt-get update && apt-get install -y ...               │
│ #8 ERROR: process "/bin/sh -c apt-get update" did not complete successfully │
│ ...                                                                         │
└──────────────────────────────────────────────────────────────────────────────┘
 Esc:back | j/k:scroll | g:top G:bottom | tail
```

**Keys in log view:**

| Key | Action |
|-----|--------|
| `j` / `k` / arrows | Scroll up/down |
| `g` / `Home` | Jump to top of log |
| `G` / `End` | Jump to bottom (tail) |
| `Esc` / `q` | Back to overview |

## Scheduling

### Work-stealing

Each node runs one build at a time. When a build finishes, the node claims the next available target from the shared queue. This naturally load-balances without needing to know anything about build sizes or node capacity — fast nodes finish sooner and grab more work.

### Platform awareness

Targets with platform constraints (e.g., `platform: linux/amd64`) are only dispatched to compatible nodes. Targets without platform constraints can run on any node. Platform variants are normalized for matching (`linux/amd64/v2` matches `linux/amd64`).

### Dependency handling

- **HCL bake files:** `depends_on` is a build-time dependency. `dbake` enforces ordering — a target won't start until its dependencies complete.
- **Compose files:** `depends_on` is a runtime startup dependency, not a build dependency. All compose targets are scheduled independently in parallel. The `--with-deps` flag uses `depends_on` only for target *selection* (expanding which targets to build), not ordering.

### Failure handling

When a target fails:
- Its dependents (HCL only) are blocked and reported as skipped, naming the dependency that failed
- With `--fail-fast`, in-flight builds are killed — the `docker buildx bake` child processes are terminated, not just left to finish
- Build logs are saved to `$TMPDIR/dbake-logs/{run-pid}/{target}.log` (the target name is sanitised for use as a filename)
- You can inspect the log in the TUI without interrupting other builds

Exit codes: `0` when every target built, `1` if any target failed or was
skipped, `130` if the run was interrupted.

`--platform` narrows each node to the platforms it actually reports, so a node
that cannot honour the constraint is dropped with a warning rather than being
handed work it cannot run.

### Cancellation

`Ctrl+C` cancels the run: in-flight builds are killed, remaining targets are reported as cancelled, and shard builders are removed. A second `Ctrl+C` exits immediately.

### Platform constraints

A target whose platform no node can satisfy is a startup error, not a hang:

```
error: no node in builder 'my-cluster' can build 1 target(s):
  arm-app requires [linux/arm64]
  available platforms: linux/amd64
```

## Shared cache with a registry

### The problem

When builds are distributed across multiple nodes, each buildkitd instance has its own local build cache. Node A builds `web` and caches its layers locally — but when node B builds `web` next time, it starts from scratch because it has never seen those layers.

Without a shared cache, distributed builds trade parallelism for redundant work. The first build is faster (parallelized), but subsequent builds lose the cache advantage.

### Using Zot as a shared cache registry

[Zot](https://zotregistry.dev/) is a lightweight OCI registry that works well as a shared build cache. Any OCI-compliant registry works (Docker Registry, Harbor, ECR, etc.), but Zot is particularly suited for homelab/on-prem setups:

- Single binary, minimal resource usage
- Native OCI artifacts support (buildkit cache manifests are OCI artifacts)
- No authentication required for local networks
- Runs on a Raspberry Pi

Example Zot setup:

```bash
# Run Zot on your network
docker run -d --name zot --restart unless-stopped \
  -p 5000:5000 \
  ghcr.io/project-zot/zot-linux-arm64:latest
```

Configure each buildkitd instance to allow HTTP access to the registry via `buildkitd.toml`:

```toml
[registry."192.168.0.110:5000"]
  http = true
  insecure = true
```

### How dbake integrates

Pass `--cache-registry` to enable shared caching:

```bash
docker dbake --cache-registry 192.168.0.110:5000
```

`dbake` adds per-target registry cache entries to each build. A single
`--set target.cache-from=` *replaces* the bake file's list rather than adding to
it, so `dbake` re-emits your file's own entries alongside the registry one —
repeated `--set` flags accumulate. The net effect is additive: nothing you
declared is lost.

```
cache-from: type=registry,ref=192.168.0.110:5000/buildcache/{target}
cache-to:   type=registry,ref=192.168.0.110:5000/buildcache/{target},mode=max
```

`mode=max` exports all layers (not just the final image layers), maximizing cache hit rates across different build stages.

### Cache flow

```
  Node A builds "web"                Node B builds "web" (later)
  ┌──────────────┐                   ┌──────────────┐
  │  buildkitd   │                   │  buildkitd   │
  │  (node A)    │                   │  (node B)    │
  └──────┬───────┘                   └──────┬───────┘
         │                                  │
    cache-to ──────►  ┌────────────┐  ◄── cache-from
                      │    Zot     │
                      │  registry  │
                      │ :5000      │
                      └────────────┘
```

1. Node A builds `web`, pushes all layers to `registry:5000/buildcache/web`
2. Node B later builds `web`, pulls cached layers from the same path
3. Only changed layers are rebuilt — even though B has never built `web` before

This is critical for distributed builds. Without it, distributing across N nodes means N times the cache misses. With a shared registry, every node benefits from every other node's work.

## Shard lifecycle

Shard builders are ephemeral and fully managed:

1. **Startup:** Shards left behind by runs that are no longer alive are cleaned up. Shards belonging to a *running* `dbake` are left alone.
2. **Creation:** One shard per running TCP node, named `{builder}-shard-{node}--run{pid}`. The run id keeps concurrent runs from clobbering each other.
3. **Cleanup:** An RAII guard removes this run's shards on exit — success, failure, or Ctrl+C. (A second Ctrl+C exits immediately and leaves cleanup to the next run.)

## Builder setup

`dbake` requires a multi-node buildx builder with TCP endpoints. Example setup with remote buildkitd instances:

```bash
# Create builder with first node
docker buildx create --name my-cluster \
  --driver remote tcp://192.168.0.129:1234

# Add more nodes
docker buildx create --name my-cluster --append \
  --driver remote tcp://192.168.0.144:1234

docker buildx create --name my-cluster --append \
  --driver remote tcp://192.168.0.110:1235

# Set as default
docker buildx use my-cluster

# Verify
docker buildx inspect my-cluster
```

Each buildkitd instance needs:
- TCP listener: `--addr tcp://0.0.0.0:<port>`
- Privileged mode (for bind mounts)
- Registry config if using an insecure/HTTP cache registry (see [above](#using-zot-as-a-shared-cache-registry))

Example buildkitd container:

```bash
docker run -d --name buildkitd --restart unless-stopped \
  --privileged \
  -p 1234:1234 \
  -v /path/to/buildkitd.toml:/etc/buildkit/buildkitd.toml \
  moby/buildkit:buildx-stable-1 \
  --addr tcp://0.0.0.0:1234 --config /etc/buildkit/buildkitd.toml
```

## Project structure

```
src/
├── main.rs              # Entry point, orchestration
├── cli.rs               # CLI flags (clap derive)
├── plugin.rs            # Docker CLI plugin metadata
├── bakeprint.rs         # docker buildx bake --print parsing
├── dag.rs               # Platform-aware DAG queue
├── builder/
│   ├── inspect.rs       # Node discovery from docker buildx inspect
│   ├── shard.rs         # Ephemeral shard builder lifecycle
│   └── node.rs          # Node/ShardNode types
├── selection.rs         # Target selection: profiles, --with-deps, --exclude
├── logs.rs              # Per-run log directory and path sanitisation
├── executor/
│   ├── bake.rs          # Build execution, cancellation and cache aggregation
│   └── progress.rs      # BuildKit --progress plain parsing
├── scheduler/
│   └── dispatcher.rs    # Work-stealing dispatcher
├── compose/
│   └── parser.rs        # Compose YAML profile filtering
└── tui/
    ├── dashboard.rs     # Interactive terminal dashboard + log viewer
    ├── fallback.rs      # Non-TTY line output
    └── state.rs         # Shared dashboard state
```

## Development

```bash
cargo build              # Debug build
cargo test               # Run tests
cargo build --release    # Release build
make install             # Build + install as Docker CLI plugin
make uninstall           # Remove from CLI plugins
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

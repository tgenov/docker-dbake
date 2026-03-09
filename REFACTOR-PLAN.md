# docker-dbake Refactor Plan

Rust code review findings, prioritized by impact. Work through top-to-bottom.

## Tier 1: Fix Now (correctness, fragility, easy wins)

### 1.1 Rewrite `platform_base()` in `src/dag.rs:27-37`

Current code allocates a Vec and uses `unwrap()`:
```rust
fn platform_base(p: &str) -> &str {
    let parts: Vec<&str> = p.split('/').collect(); // unnecessary Vec
    if parts.len() >= 2 {
        let end = p.find('/').unwrap() + 1 + parts[1].len(); // unwrap
        &p[..end]
    } else {
        p
    }
}
```

Replace with:
```rust
fn platform_base(p: &str) -> &str {
    match p.find('/') {
        Some(first_slash) => {
            let rest = &p[first_slash + 1..];
            match rest.find('/') {
                Some(second_slash) => &p[..first_slash + 1 + second_slash],
                None => p, // only "os/arch", no variant
            }
        }
        None => p,
    }
}
```

Zero allocations, no unwrap. Existing tests cover this.

### 1.2 `context(format!(...))` → `with_context(|| format!(...))`

Defers string allocation to error path only. Grep for `context(format!` and replace all:

- `src/main.rs:63` — `discover_nodes` error
- `src/bakeprint.rs:78` — `bake_print` Command error
- `src/bakeprint.rs:98` — `read_to_string` error
- `src/executor/bake.rs:36` — shard creation error
- `src/executor/bake.rs:101` — log file creation error
- `src/executor/bake.rs:110` — bake execution error
- `src/compose/parser.rs:22` — compose read error
- `src/builder/shard.rs:27` — shard creation error

Pattern: `.context(format!("...", x))` → `.with_context(|| format!("...", x))`

### 1.3 Remove stale `#[allow(dead_code)]`

- `src/builder/node.rs:2` — `Node` struct IS used, remove the annotation
- `src/bakeprint.rs:8,51,60` — these are justified (serde fields not accessed in code) but add a comment explaining why

### 1.4 `cli.rs`: `file: String` → `PathBuf`

Change `src/cli.rs:10-11`:
```rust
#[arg(short = 'f', long = "file", default_value = "docker-compose.yml")]
pub file: PathBuf,
```

Then update all `&cli.file` and `cli.file.clone()` references in main.rs to use `.to_str().unwrap()` or `.as_path()` where needed. `bake_print()` and `extract_depends_on()` take `&str`, so pass `cli.file.to_str().unwrap()` or change their signatures to `&Path`.

### 1.5 Handle mutex poisoning consistently

All `lock().unwrap()` calls should use a helper or `unwrap_or_else`:

Add to `src/tui/state.rs` or a util module:
```rust
use std::sync::{Mutex, MutexGuard};

pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
```

Affected files:
- `src/scheduler/dispatcher.rs` — 7 lock sites
- `src/main.rs` — 2 lock sites
- `src/tui/dashboard.rs` — 5 lock sites

Note: `std::sync::Mutex` is correct here (not tokio::sync::Mutex) because critical sections are tiny and the TUI thread uses `spawn_blocking`. Add a comment at `SharedDag` explaining this choice.

## Tier 2: Reduce Allocations (medium impact)

### 2.1 Eliminate unnecessary clones in `main.rs` arg handling

Lines 42-49: Don't collect args twice.
```rust
// Before:
let args: Vec<String> = std::env::args().collect();
if args.iter().any(|a| a == "docker-cli-plugin-metadata") { ... }
let filtered_args: Vec<String> = args.into_iter().filter(|a| a != "dbake").collect();

// After:
let args: Vec<String> = std::env::args().collect();
if args.iter().any(|a| a == "docker-cli-plugin-metadata") {
    plugin::print_metadata();
    return Ok(());
}
let cli = Cli::parse_from(args.iter().filter(|a| *a != "dbake"));
```

### 2.2 Reduce clones in `--with-deps` expansion (main.rs:119-137)

Three collections all cloned from `target_names`. Refactor:
```rust
let all_bake_targets: HashSet<&str> = bake_print.target.keys().map(|s| s.as_str()).collect();
let mut expanded: HashSet<String> = target_names.iter().cloned().collect();
let mut queue: VecDeque<&str> = target_names.iter().map(|s| s.as_str()).collect();

while let Some(t) = queue.pop_front() {
    if let Some(dep_list) = all_deps.get(t) {
        for dep in dep_list {
            if all_bake_targets.contains(dep.as_str()) && expanded.insert(dep.clone()) {
                queue.push_back(dep.as_str());
            }
        }
    }
}
```

### 2.3 `DagQueue::new()` — avoid double-cloning targets

`src/dag.rs:58-104`: `new()` takes `targets: Vec<String>` by value but then clones everything into HashMaps. Refactor to consume the Vec:

```rust
pub fn new(
    targets: Vec<String>,
    deps: HashMap<String, Vec<String>>,
    target_platforms: HashMap<String, Vec<String>>,
) -> Self {
    let all: HashSet<String> = targets.iter().cloned().collect();
    // ... rest uses all for lookups, targets for iteration
```

The `targets` Vec is consumed but then `iter().cloned()` creates copies anyway. Instead, build `all` from `targets` directly and iterate `all`:

```rust
let all: HashSet<String> = targets.into_iter().collect();
// Use &all for iteration instead of &targets
```

This saves N string clones where N = number of targets.

### 2.4 `bakeprint.rs` — return `&str` instead of `String` where possible

- `extract_quoted_string()` line 195: return `Option<&str>` instead of `Option<String>`
- `parse_hcl_string_list()` line 204: return `Vec<&str>` instead of `Vec<String>` (callers will need to `.to_string()` if they need owned)
- `CacheEntry::to_arg()` line 41: return `Cow<'_, str>` instead of `String`

These cascade — the callers that need owned strings will clone, but many don't.

## Tier 3: Style & Ergonomics (low impact, nice to have)

### 3.1 Use imports instead of fully-qualified paths in main.rs

Replace scattered `std::collections::HashSet`, `std::collections::VecDeque` with `use` at top:
```rust
use std::collections::{HashSet, VecDeque};
```

### 3.2 Add `Default` derives where useful

- `src/executor/bake.rs` — `TargetCacheConfig` (defaults to empty vecs)

### 3.3 Consistent error handling style

Some functions use `bail!()`, others use `Err(anyhow!(...))`, others use `.context()`. Pick one style per error type:
- Parse failures: `.context()` / `.with_context()`
- Validation failures: `bail!()`
- Command failures: `bail!()` with stderr

### 3.4 Add `#[must_use]` to pure functions

- `dag.rs`: `is_done()`, `is_stalled()`, `blocked_targets()`, `ready_count()`
- `tui/state.rs`: all `count_*()` methods, `is_complete()`, `elapsed()`

## Not Changing (deliberate decisions)

- **`std::sync::Mutex` in async context**: Critical sections are <1μs (queue pop, status update). `tokio::sync::Mutex` would require `.await` which is incompatible with the `spawn_blocking` TUI thread. This is the correct choice.
- **Owned `String` in `DagQueue` fields**: The DAG outlives all input data and is shared across threads via `Arc`. Lifetimes would add complexity with no real benefit for this use case.
- **`serde_yaml` deprecation warning**: The crate works fine. Migration to `serde_yml` or similar can happen later.

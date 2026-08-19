mod bakeprint;
mod builder;
mod cli;
mod compose;
mod dag;
mod executor;
mod logs;
mod plugin;
mod scheduler;
mod selection;
mod tui;
mod utils;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use builder::node::ShardNode;
use cli::{Cli, Invocation};
use executor::bake::{BakeConfig, TargetCacheConfig};
use selection::{select_targets, SelectionInput};
use tui::state::DashboardState;

/// Read the active docker buildx builder name from ~/.docker/buildx/current.
fn default_builder() -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = std::path::PathBuf::from(home).join(".docker/buildx/current");
    let data = std::fs::read_to_string(&path)
        .context("cannot read ~/.docker/buildx/current — specify --builder explicitly")?;
    let json: serde_json::Value =
        serde_json::from_str(&data).context("invalid JSON in ~/.docker/buildx/current")?;
    json["Name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .context("no active builder set — specify --builder explicitly")
}

/// How the run ended.
enum Outcome {
    Success,
    Failed,
    /// Interrupted by the user; reported with the conventional signal code.
    Cancelled,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match cli::normalize_args(std::env::args().collect()) {
        Invocation::Metadata => {
            plugin::print_metadata();
            return ExitCode::SUCCESS;
        }
        Invocation::Run(args) => Cli::parse_from(args),
    };

    // All RAII guards live inside `run`, so they are dropped before the process
    // exits — calling `std::process::exit` here would skip shard cleanup.
    match run(cli).await {
        Ok(Outcome::Success) => ExitCode::SUCCESS,
        Ok(Outcome::Failed) => ExitCode::FAILURE,
        // 128 + SIGINT, matching what a shell reports for an interrupted child.
        Ok(Outcome::Cancelled) => ExitCode::from(130),
        Err(e) => {
            eprintln!("error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<Outcome> {
    let builder_name = match cli.builder.clone() {
        Some(b) => b,
        None => default_builder()?,
    };

    let run_id = std::process::id();

    // Clean shards left behind by runs that are no longer alive
    builder::shard::cleanup_stale_shards(&builder_name)?;

    // Discover nodes
    let nodes = builder::inspect::discover_nodes(&builder_name)
        .with_context(|| format!("failed to inspect builder '{}'", builder_name))?;

    if nodes.is_empty() {
        anyhow::bail!("no TCP nodes found in builder '{}'", builder_name);
    }

    eprintln!(
        "Builder: {} ({} nodes: {})",
        builder_name,
        nodes.len(),
        nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Dependency edges come from the source file: `bake --print` resolves
    // targets but does not report depends_on, and does not pull dependencies in
    // when given an explicit target.
    // For compose YAML, depends_on is runtime startup order (not build order),
    // so we only use it for --with-deps target expansion, not DAG scheduling.
    // For HCL bake files, depends_on IS a build dependency.
    let file_contents = std::fs::read_to_string(&cli.file).ok();
    let is_hcl = selection::is_hcl_bake_file(&cli.file, file_contents.as_deref());
    let all_deps = bakeprint::extract_depends_on(&cli.file)?;

    // Compose profiles, when the bake file is a compose file. A malformed
    // compose file is an error here, not a silently absent profile list.
    let profiles: Option<HashMap<String, Vec<String>>> = if is_hcl {
        None
    } else {
        Some(compose::parser::parse_compose(&cli.file)?.profiles())
    };

    // --- Target discovery via `docker buildx bake --print` ---
    // Expand the dependency chain first: buildx prints only the targets it is
    // asked for, so requesting `app` alone would hide `base` and silently turn
    // --with-deps into a no-op.
    // Only pre-expand for HCL. compose `depends_on` names runtime services that
    // often have no `build:` section, so feeding them to buildx would ask it to
    // resolve targets that do not exist.
    let wanted = if cli.with_deps && is_hcl {
        selection::expand_deps(&cli.targets, &all_deps)
    } else {
        cli.targets.clone()
    };
    let bake_print = bakeprint::bake_print(&cli.file, &builder_name, &wanted)?;

    let mut all_target_names: Vec<String> = bake_print.target.keys().cloned().collect();
    all_target_names.sort();

    let selection = select_targets(SelectionInput {
        all_targets: &all_target_names,
        deps: &all_deps,
        profiles: profiles.as_ref(),
        profiles_wanted: &cli.profile,
        requested: &cli.targets,
        exclude: &cli.exclude,
        with_deps: cli.with_deps,
        // compose depends_on is runtime ordering, so "you excluded a build
        // dependency" is not a meaningful warning there.
        deps_are_build_deps: is_hcl,
    })?;

    for warning in &selection.warnings {
        eprintln!("warning: {}", warning);
    }
    let target_names = selection.targets;

    // Per-target cache config from the bake file. These entries must be
    // re-emitted alongside the registry cache, because a single
    // `--set target.cache-from=` replaces the file's list rather than adding
    // to it.
    let mut cache_configs: HashMap<String, TargetCacheConfig> = HashMap::new();
    for name in &target_names {
        if let Some(bt) = bake_print.target.get(name) {
            if !bt.cache_from.is_empty() || !bt.cache_to.is_empty() {
                cache_configs.insert(
                    name.clone(),
                    TargetCacheConfig {
                        cache_from: bt.cache_from.clone(),
                        cache_to: bt.cache_to.clone(),
                    },
                );
            }
        }
    }

    // Build dependency edges scoped to our target set.
    // Only enforce build ordering for HCL bake files — compose depends_on is runtime-only.
    let scoped_deps: HashMap<String, Vec<String>> = if is_hcl {
        all_deps
            .into_iter()
            .filter(|(k, _)| target_names.contains(k))
            .collect()
    } else {
        HashMap::new()
    };

    // Extract per-target platforms from bake --print
    let mut target_platforms: HashMap<String, Vec<String>> = HashMap::new();
    for name in &target_names {
        if let Some(bt) = bake_print.target.get(name) {
            if !bt.platforms.is_empty() {
                target_platforms.insert(name.clone(), bt.platforms.clone());
            }
        }
    }

    // Print the DAG
    let independent_count = target_names
        .iter()
        .filter(|t| scoped_deps.get(*t).is_none_or(|d| d.is_empty()))
        .count();
    let has_deps = target_names.len() - independent_count;

    eprintln!(
        "Targets: {} ({} independent, {} with dependencies)",
        target_names.len(),
        independent_count,
        has_deps
    );

    if has_deps > 0 {
        for (target, deps) in &scoped_deps {
            if !deps.is_empty() && target_names.contains(target) {
                eprintln!("  {} → depends on [{}]", target, deps.join(", "));
            }
        }
    }

    // Show platform constraints
    for (target, plats) in target_platforms
        .iter()
        .filter(|(t, _)| target_names.contains(t))
    {
        eprintln!("  {} → platform [{}]", target, plats.join(", "));
    }

    if !cache_configs.is_empty() && cli.cache_registry.is_some() {
        eprintln!("  registry cache added alongside the bake file's own entries for:");
        let mut named: Vec<&String> = cache_configs.keys().collect();
        named.sort();
        for name in named {
            let cache = &cache_configs[name];
            let entries: Vec<String> = cache
                .cache_from
                .iter()
                .chain(cache.cache_to.iter())
                .map(|e| e.to_arg())
                .collect();
            eprintln!("    {} → [{}]", name, entries.join("; "));
        }
    }

    // `--platform` changes what each shard advertises to buildx, so it must
    // drive scheduling too — otherwise targets are matched against platforms
    // the shard no longer reports.
    //
    // It narrows rather than replaces: a node can only build what it actually
    // supports, and `buildx inspect` already lists emulated platforms. Treating
    // the flag as a replacement would let us schedule work onto a node that
    // cannot run it, and would silence the unbuildable-target check below.
    let platform_override: Option<Vec<String>> = match cli.platform.as_deref() {
        None => None,
        Some(raw) => {
            let parsed: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if parsed.is_empty() {
                anyhow::bail!("--platform '{}' names no platforms", raw);
            }
            Some(parsed)
        }
    };

    let effective_platforms = |node: &builder::node::Node| -> Vec<String> {
        match &platform_override {
            None => node.platforms.clone(),
            // A node that reports no platforms at all tells us nothing, so take
            // it at its word rather than excluding it — it was usable before.
            Some(wanted) if node.platforms.is_empty() => wanted.clone(),
            // Compare with the same rule the scheduler uses, which honours an
            // explicit variant instead of collapsing it to the base arch.
            Some(wanted) => node
                .platforms
                .iter()
                .filter(|p| wanted.iter().any(|w| dag::platform_matches(w, p)))
                .cloned()
                .collect(),
        }
    };

    if let Some(ref wanted) = platform_override {
        let unusable: Vec<&builder::node::Node> = nodes
            .iter()
            .filter(|n| effective_platforms(n).is_empty())
            .collect();
        if unusable.len() == nodes.len() {
            anyhow::bail!(
                "no node in builder '{}' supports --platform {}\n  nodes report: {}",
                builder_name,
                wanted.join(","),
                nodes
                    .iter()
                    .map(|n| format!("{} [{}]", n.name, n.platforms.join(", ")))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        for node in unusable {
            eprintln!(
                "warning: node {} does not support --platform {} — skipping it",
                node.name,
                wanted.join(",")
            );
        }
    }

    // Nodes that cannot honour the platform constraint take no part in the run.
    // Only ever applied when --platform was given: a node that simply reports
    // no platforms at all was usable before and must stay usable.
    let nodes: Vec<builder::node::Node> = if platform_override.is_some() {
        nodes
            .into_iter()
            .filter(|n| !effective_platforms(n).is_empty())
            .collect()
    } else {
        nodes
    };

    if nodes.is_empty() {
        anyhow::bail!("no usable nodes left in builder '{}'", builder_name);
    }
    if nodes.len() == 1 {
        eprintln!("note: only one node is usable — builds will run sequentially");
    }

    // Build the DAG queue, telling it which nodes exist so unbuildable targets
    // are an error rather than a hang.
    let node_platform_lists: Vec<Vec<String>> = nodes.iter().map(effective_platforms).collect();
    let dag = dag::DagQueue::new(target_names.clone(), scoped_deps, target_platforms)
        .with_nodes(node_platform_lists.clone());

    let unsatisfiable = dag.unsatisfiable_targets();
    if !unsatisfiable.is_empty() {
        let mut available: Vec<String> = node_platform_lists.into_iter().flatten().collect();
        available.sort();
        available.dedup();
        let detail = unsatisfiable
            .iter()
            .map(|(t, p)| format!("  {} requires [{}]", t, p.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "no node in builder '{}' can build {} target(s):\n{}\n  available platforms: {}",
            builder_name,
            unsatisfiable.len(),
            detail,
            available.join(", ")
        );
    }

    // Create shard builders. The guard exists before the first shard does, so a
    // failure part-way through the loop still cleans up the ones already made.
    let mut shard_guard = builder::shard::ShardGuard::new();
    let mut shard_nodes = Vec::new();

    for node in &nodes {
        // Pass the intersection, not the raw flag: `buildx create --platform`
        // sets *fixed* platforms for the node, so handing it the full --platform
        // list would make the shard advertise architectures the scheduler never
        // routes to it — and unconstrained targets inherit that advertisement.
        let node_platforms = effective_platforms(node);
        let shard_platform = platform_override.as_ref().map(|_| node_platforms.join(","));
        let shard =
            builder::shard::create_shard(&builder_name, node, shard_platform.as_deref(), run_id)?;
        shard_guard.push(shard.clone());
        eprintln!(
            "Created shard: {} → {} [{}]",
            shard,
            node.endpoint,
            node_platforms.join(", ")
        );
        shard_nodes.push(ShardNode {
            name: node.name.clone(),
            shard_builder: shard,
            platforms: node_platforms,
        });
    }

    // Log directory for this run
    let run_dir = logs::run_log_dir(run_id);
    logs::create_run_dir(&run_dir)?;
    eprintln!("Logs: {}", run_dir.display());

    // Set up cancellation. The first Ctrl+C winds down gracefully and lets the
    // shard guard run; a second one is a deliberate escape hatch that exits
    // immediately, skipping cleanup — the next run reclaims those shards.
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        eprintln!("\nInterrupted — cancelling builds and cleaning up shard builders...");
        cancel_clone.cancel();

        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nSecond interrupt — exiting immediately.");
            std::process::exit(130);
        }
    });

    // Determine progress mode for bake subcommands
    let bake_progress = if cli.progress == "auto" {
        "plain".to_string() // We manage our own TUI
    } else {
        cli.progress.clone()
    };

    let config = BakeConfig {
        file: cli.file,
        progress: bake_progress,
        cache_registry: cli.cache_registry.clone(),
        no_cache: cli.no_cache,
        load: cli.load,
        push: cli.push,
        fail_fast: cli.fail_fast,
    };

    // Dashboard state
    let node_names: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
    let state = Arc::new(Mutex::new(DashboardState::new(
        target_names.clone(),
        node_names,
    )));

    // Decide on TUI vs fallback
    let use_tui = std::io::stdout().is_terminal() && cli.progress != "plain";

    eprintln!("---");
    eprintln!(
        "Dispatching {} targets across {} nodes (1 build/node)...",
        target_names.len(),
        nodes.len(),
    );
    eprintln!("---");

    if use_tui {
        let dispatch_state = state.clone();
        let dispatch_cancel = cancel.clone();
        let dispatch_run_dir = run_dir.clone();
        let dispatch_cache = cache_configs;
        let dispatch_handle = tokio::spawn(async move {
            scheduler::dispatcher::dispatch(
                shard_nodes,
                dag,
                config,
                dispatch_cache,
                dispatch_run_dir,
                dispatch_state,
                dispatch_cancel,
            )
            .await
        });

        let tui_state = state.clone();
        let tui_cancel = cancel.clone();
        let tui_result = tokio::task::spawn_blocking(move || {
            let mut dashboard = tui::dashboard::Dashboard::new(tui_state, tui_cancel)?;
            dashboard.run()
        })
        .await?;

        if let Err(e) = tui_result {
            eprintln!("TUI error: {}", e);
            // Dropping the handle would only detach the dispatcher, leaving
            // builds running while the shard guard removes their shards.
            cancel.cancel();
        }

        dispatch_handle.await??;
    } else {
        let fallback_state = state.clone();
        let dispatch_state = state.clone();
        let dispatch_cancel = cancel.clone();

        let (dispatch_result, _) = tokio::join!(
            scheduler::dispatcher::dispatch(
                shard_nodes,
                dag,
                config,
                cache_configs,
                run_dir.clone(),
                dispatch_state,
                dispatch_cancel,
            ),
            tui::fallback::run_fallback(fallback_state, cancel.clone()),
        );

        dispatch_result?;
    }

    // Print summary
    let s = utils::lock_or_recover(&state);
    let failed = s.failed_targets();

    if !failed.is_empty() {
        eprintln!("\nFAILED targets:");
        for t in &failed {
            if let tui::state::TargetStatus::Failed(ref reason) = t.status {
                eprintln!("  - {} ({})", t.name, reason);
            }
        }
        eprintln!("Logs: {}", run_dir.display());
        return Ok(Outcome::Failed);
    }

    let cancelled = cancel.is_cancelled();
    let skipped = s.count_skipped();
    let elapsed = s.elapsed();
    eprintln!(
        "\n{} of {} targets built across {} nodes in {}m {:02}s.{}",
        s.count_done(),
        s.total,
        s.node_names.len(),
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60,
        if skipped > 0 {
            format!(" {} skipped.", skipped)
        } else {
            String::new()
        }
    );

    // A Ctrl+C that lands after the last build finished did not interrupt
    // anything, so it must not turn a clean run into exit 130.
    if cancelled && (skipped > 0 || s.count_done() < s.total) {
        return Ok(Outcome::Cancelled);
    }
    if skipped > 0 {
        return Ok(Outcome::Failed);
    }

    Ok(Outcome::Success)
}

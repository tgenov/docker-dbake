mod bakeprint;
mod builder;
mod cli;
mod compose;
mod dag;
mod executor;
mod plugin;
mod scheduler;
mod tui;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use builder::node::ShardNode;
use cli::Cli;
use executor::bake::{BakeConfig, TargetCacheConfig};
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

#[tokio::main]
async fn main() -> Result<()> {
    // Docker CLI plugin protocol: respond to metadata subcommand
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "docker-cli-plugin-metadata") {
        plugin::print_metadata();
        return Ok(());
    }

    // Strip "dbake" if invoked as `docker dbake` (docker passes plugin name as argv[1])
    let filtered_args: Vec<String> = args.into_iter().filter(|a| a != "dbake").collect();

    let cli = Cli::parse_from(&filtered_args);

    let builder_name = match cli.builder {
        Some(b) => b,
        None => default_builder()?,
    };

    // Clean stale shards from previous crashes
    builder::shard::cleanup_stale_shards(&builder_name)?;

    // Discover nodes
    let nodes = builder::inspect::discover_nodes(&builder_name)
        .context(format!("failed to inspect builder '{}'", builder_name))?;

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

    // --- Target discovery via `docker buildx bake --print` ---
    // This is the canonical source of truth for both HCL and compose files.
    let bake_print = bakeprint::bake_print(&cli.file, &builder_name)?;

    // Extract dependency edges from the source file (bake --print doesn't include depends_on).
    // For compose YAML, depends_on is runtime startup order (not build order),
    // so we only use it for --with-deps target expansion, not DAG scheduling.
    // For HCL bake files, depends_on IS a build dependency.
    let is_hcl = cli.file.ends_with(".hcl")
        || std::fs::read_to_string(&cli.file)
            .map(|c| c.contains("target \""))
            .unwrap_or(false);
    let all_deps = bakeprint::extract_depends_on(&cli.file)?;

    // Determine which targets to build
    let mut target_names: Vec<String> = bake_print.target.keys().cloned().collect();
    target_names.sort();

    // Filter by profile if using compose YAML
    if let Some(ref profile) = cli.profile {
        let compose = compose::parser::parse_compose(std::path::Path::new(&cli.file))?;
        let profile_services: std::collections::HashSet<String> = compose
            .services
            .iter()
            .filter(|(_, svc)| svc.profiles.contains(profile))
            .map(|(name, _)| name.clone())
            .collect();
        target_names.retain(|t| profile_services.contains(t));
    }

    // If explicit targets specified, use only those
    if !cli.targets.is_empty() {
        let requested: std::collections::HashSet<&str> =
            cli.targets.iter().map(|s| s.as_str()).collect();
        target_names.retain(|t| requested.contains(t.as_str()));
    }

    // If --with-deps, expand dependency chain
    if cli.with_deps {
        let all_bake_targets: std::collections::HashSet<String> =
            bake_print.target.keys().cloned().collect();
        let mut expanded: std::collections::HashSet<String> =
            target_names.iter().cloned().collect();
        let mut queue: std::collections::VecDeque<String> = target_names.iter().cloned().collect();

        while let Some(t) = queue.pop_front() {
            if let Some(dep_list) = all_deps.get(&t) {
                for dep in dep_list {
                    if all_bake_targets.contains(dep) && expanded.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        target_names = expanded.into_iter().collect();
        target_names.sort();
    }

    // Apply exclude filter
    if !cli.exclude.is_empty() {
        let exclude_set: std::collections::HashSet<&str> =
            cli.exclude.iter().map(|s| s.as_str()).collect();
        target_names.retain(|t| !exclude_set.contains(t.as_str()));
    }

    if target_names.is_empty() {
        anyhow::bail!("no targets to build after filtering");
    }

    // Extract per-target cache configs from bake --print output
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
        .filter(|t| scoped_deps.get(*t).map_or(true, |d| d.is_empty()))
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
    let platformed: Vec<_> = target_platforms
        .iter()
        .filter(|(t, _)| target_names.contains(t))
        .collect();
    if !platformed.is_empty() {
        for (target, plats) in &platformed {
            eprintln!("  {} → platform [{}]", target, plats.join(", "));
        }
    }

    // Build the DAG queue
    let dag = dag::DagQueue::new(target_names.clone(), scoped_deps, target_platforms);

    // Create shard builders
    let mut shard_names = Vec::new();
    let mut shard_nodes = Vec::new();

    for node in &nodes {
        let shard = builder::shard::create_shard(&builder_name, node, cli.platform.as_deref())?;
        eprintln!(
            "Created shard: {} → {} [{}]",
            shard,
            node.endpoint,
            node.platforms.join(", ")
        );
        shard_nodes.push(ShardNode {
            name: node.name.clone(),
            shard_builder: shard.clone(),
            platforms: node.platforms.clone(),
        });
        shard_names.push(shard);
    }

    // RAII guard to clean up shards on exit
    let _shard_guard = builder::shard::ShardGuard::new(shard_names);

    // Set up cancellation
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nInterrupted — cleaning up shard builders...");
        cancel_clone.cancel();
    });

    // Determine progress mode for bake subcommands
    let bake_progress = if cli.progress == "auto" {
        "plain".to_string() // We manage our own TUI
    } else {
        cli.progress.clone()
    };

    let config = BakeConfig {
        file: cli.file.clone(),
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
        let dispatch_handle = tokio::spawn(async move {
            scheduler::dispatcher::dispatch(
                shard_nodes,
                dag,
                config,
                cache_configs,
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
                dispatch_state,
                dispatch_cancel,
            ),
            tui::fallback::run_fallback(fallback_state),
        );

        dispatch_result?;
    }

    // Print summary
    let s = state.lock().unwrap();
    let failed = s.failed_targets();

    if !failed.is_empty() {
        eprintln!("\nFAILED targets:");
        for t in &failed {
            if let tui::state::TargetStatus::Failed(ref log) = t.status {
                eprintln!("  - {} ({})", t.name, log);
            }
        }
        std::process::exit(1);
    }

    let elapsed = s.elapsed();
    eprintln!(
        "\nAll {} targets built across {} nodes in {}m {:02}s.",
        s.total,
        s.node_names.len(),
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60,
    );

    Ok(())
}

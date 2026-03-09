use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::process::Command;

use crate::bakeprint::CacheEntry;

/// Global configuration for bake execution, shared across all targets.
#[derive(Debug, Clone)]
pub struct BakeConfig {
    pub file: String,
    pub progress: String,
    pub cache_registry: Option<String>,
    pub no_cache: bool,
    pub load: bool,
    pub push: bool,
    pub fail_fast: bool,
}

/// Per-target cache configuration extracted from `docker buildx bake --print`.
#[derive(Debug, Clone)]
pub struct TargetCacheConfig {
    pub cache_from: Vec<CacheEntry>,
    pub cache_to: Vec<CacheEntry>,
}

/// Execute `docker buildx bake` for a single target on a specific shard builder.
///
/// Cache handling:
/// - The bake file's own cache-from/to are included by default (bake reads them).
/// - If `--cache-registry` is set, we APPEND a registry cache source/dest via `--set`,
///   so both the file's caches and our registry cache are used.
/// - We use `--set *.cache-from=` which appends to the list, not replaces.
pub async fn execute_bake(
    builder: &str,
    config: &BakeConfig,
    target: &str,
    target_cache: Option<&TargetCacheConfig>,
) -> Result<std::path::PathBuf> {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "bake"]);
    cmd.args(["--builder", builder]);
    cmd.args(["-f", &config.file]);
    cmd.args(["--progress", &config.progress]);

    // Aggregate cache: file-level cache entries are already in the bake file.
    // We add registry cache on top if --cache-registry is specified.
    if let Some(ref registry) = config.cache_registry {
        // If the target already has cache-from entries in the file, bake will
        // use them. We append our registry cache via --set which merges.
        let registry_from = format!(
            "type=registry,ref={}/buildcache/{}",
            registry, target
        );
        let registry_to = format!(
            "type=registry,ref={}/buildcache/{},mode=max",
            registry, target
        );

        // Collect all cache-from sources: existing file entries + registry
        if let Some(tc) = target_cache {
            // Re-emit the file's cache-from entries so --set doesn't clobber them
            let mut all_from: Vec<String> = tc.cache_from.iter().map(|e| e.to_arg()).collect();
            all_from.push(registry_from);

            for entry in &all_from {
                cmd.args(["--set", &format!("{}.cache-from={}", target, entry)]);
            }

            // Same for cache-to
            let mut all_to: Vec<String> = tc.cache_to.iter().map(|e| e.to_arg()).collect();
            all_to.push(registry_to);

            for entry in &all_to {
                cmd.args(["--set", &format!("{}.cache-to={}", target, entry)]);
            }
        } else {
            // No file-level cache — just add registry cache
            cmd.args([
                "--set",
                &format!("{}.cache-from={}", target, registry_from),
            ]);
            cmd.args([
                "--set",
                &format!("{}.cache-to={}", target, registry_to),
            ]);
        }
    }

    if config.no_cache {
        cmd.arg("--no-cache");
    }

    if config.load {
        cmd.arg("--load");
    }

    if config.push {
        cmd.arg("--push");
    }

    cmd.arg(target);

    // Capture output to a temp log file
    let log_dir = std::env::temp_dir().join("dbake-logs");
    tokio::fs::create_dir_all(&log_dir).await.ok();
    let log_path = log_dir.join(format!("{}.log", target));

    let log_file = std::fs::File::create(&log_path)
        .context(format!("failed to create log file {}", log_path.display()))?;
    let log_file_err = log_file.try_clone()?;

    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file_err));

    let status = cmd
        .status()
        .await
        .context(format!("failed to execute bake for target {}", target))?;

    if !status.success() {
        bail!(
            "bake failed for target {} (log: {})",
            target,
            log_path.display()
        );
    }

    Ok(log_path)
}

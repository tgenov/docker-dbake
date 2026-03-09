use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

use crate::bakeprint::CacheEntry;
use crate::tui::state::DashboardState;

use super::progress::ProgressParser;

/// Global configuration for bake execution, shared across all targets.
#[derive(Debug, Clone)]
pub struct BakeConfig {
    pub file: std::path::PathBuf,
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
/// Streams stderr in real-time to parse BuildKit progress output, while teeing
/// both stdout and stderr to a log file. Progress updates are pushed to the
/// dashboard state via the `on_progress` callback.
pub async fn execute_bake(
    builder: &str,
    config: &BakeConfig,
    target: &str,
    target_cache: Option<&TargetCacheConfig>,
    target_platforms: &[String],
    state: Option<Arc<Mutex<DashboardState>>>,
    target_name: String,
) -> Result<std::path::PathBuf> {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "bake"]);
    cmd.args(["--builder", builder]);
    cmd.args(["-f", &config.file.to_string_lossy()]);
    cmd.args(["--progress", &config.progress]);

    // Force the target's platform constraint so buildkit pulls the correct
    // architecture, even when running on a multi-arch or foreign-arch node.
    if !target_platforms.is_empty() {
        let platform_value = target_platforms.join(",");
        cmd.args(["--set", &format!("{}.platform={}", target, platform_value)]);
    }

    // Aggregate cache: file-level cache entries are already in the bake file.
    // We add registry cache on top if --cache-registry is specified.
    if let Some(ref registry) = config.cache_registry {
        let registry_from = format!("type=registry,ref={}/buildcache/{}", registry, target);
        let registry_to = format!(
            "type=registry,ref={}/buildcache/{},mode=max",
            registry, target
        );

        if let Some(tc) = target_cache {
            let mut all_from: Vec<String> = tc.cache_from.iter().map(|e| e.to_arg()).collect();
            all_from.push(registry_from);

            for entry in &all_from {
                cmd.args(["--set", &format!("{}.cache-from={}", target, entry)]);
            }

            let mut all_to: Vec<String> = tc.cache_to.iter().map(|e| e.to_arg()).collect();
            all_to.push(registry_to);

            for entry in &all_to {
                cmd.args(["--set", &format!("{}.cache-to={}", target, entry)]);
            }
        } else {
            cmd.args(["--set", &format!("{}.cache-from={}", target, registry_from)]);
            cmd.args(["--set", &format!("{}.cache-to={}", target, registry_to)]);
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

    // Set up log file
    let log_dir = std::env::temp_dir().join("dbake-logs");
    tokio::fs::create_dir_all(&log_dir).await.ok();
    let log_path = log_dir.join(format!("{}.log", target));

    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;
    let log_file = Arc::new(Mutex::new(log_file));

    // Pipe stdout and stderr for real-time progress parsing
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to execute bake for target {}", target))?;

    let stderr = child.stderr.take().expect("stderr piped");
    let stdout = child.stdout.take().expect("stdout piped");

    // Spawn stderr reader — this is where BuildKit progress goes
    let log_clone = log_file.clone();
    let stderr_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut parser = ProgressParser::new();
        while let Ok(Some(line)) = lines.next_line().await {
            // Tee to log file
            if let Ok(mut f) = log_clone.lock() {
                use std::io::Write;
                writeln!(f, "{}", line).ok();
            }
            // Parse for progress updates
            if let Some(progress) = parser.parse_line(&line) {
                if let Some(ref state) = state {
                    let mut dashboard = state.lock().unwrap_or_else(|p| p.into_inner());
                    dashboard.update_progress(&target_name, progress);
                }
            }
        }
    });

    // Spawn stdout reader — just tee to log
    let log_clone2 = log_file.clone();
    let stdout_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut f) = log_clone2.lock() {
                use std::io::Write;
                writeln!(f, "{}", line).ok();
            }
        }
    });

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for bake for target {}", target))?;

    // Wait for output readers to finish
    stderr_handle.await.ok();
    stdout_handle.await.ok();

    if !status.success() {
        bail!(
            "bake failed for target {} (log: {})",
            target,
            log_path.display()
        );
    }

    Ok(log_path)
}

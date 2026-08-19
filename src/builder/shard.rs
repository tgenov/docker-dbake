use anyhow::{Context, Result};
use std::process::Command;

use super::node::Node;

/// Name of the ephemeral shard builder for a node within one run.
///
/// The run id (the process id) keeps concurrent `dbake` invocations from
/// creating — and then destroying — each other's shards.
pub fn shard_name(builder_prefix: &str, node_name: &str, run_id: u32) -> String {
    format!(
        "{}-shard-{}{}{}",
        builder_prefix, node_name, RUN_MARKER, run_id
    )
}

/// Separates the node name from the run id. A plain `-` would be ambiguous:
/// a legacy shard for a node called `worker-2` would parse as "owned by pid 2".
const RUN_MARKER: &str = "--run";

/// What a builder name listed by `docker buildx ls` is, relative to our prefix.
#[derive(Debug, PartialEq, Eq)]
pub enum ShardKind {
    /// Not one of our shards.
    Foreign,
    /// A shard from a version that did not tag names with a run id.
    Legacy,
    /// A shard belonging to the run with this id.
    Owned(u32),
}

/// Classify a builder name so cleanup can leave live runs alone.
pub fn classify_shard(name: &str, builder_prefix: &str) -> ShardKind {
    let prefix = format!("{}-shard-", builder_prefix);
    let Some(rest) = name.strip_prefix(&prefix) else {
        return ShardKind::Foreign;
    };
    match rest
        .rsplit_once(RUN_MARKER)
        .and_then(|(_, pid)| pid.parse::<u32>().ok())
    {
        Some(pid) => ShardKind::Owned(pid),
        None => ShardKind::Legacy,
    }
}

/// Whether a process is still running, so we never delete a live run's shard.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // Signal 0 performs error checking without sending a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Without a way to check, assume alive: deleting a shard out from under a
    // live run breaks that build, while leaving one behind is only untidy.
    true
}

/// Create an ephemeral shard builder for a single node.
/// Returns the shard builder name.
pub fn create_shard(
    builder_prefix: &str,
    node: &Node,
    platform: Option<&str>,
    run_id: u32,
) -> Result<String> {
    let shard_name = shard_name(builder_prefix, &node.name, run_id);

    // Reclaim our own name if a previous run with this pid died hard: cleanup
    // deliberately skips shards whose pid is alive, and the live pid it sees
    // first is our own, so nothing else will remove it.
    let _ = Command::new("docker")
        .args(["buildx", "rm", &shard_name])
        .output();

    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "create", "--name", &shard_name]);

    if let Some(plat) = platform {
        cmd.args(["--platform", plat]);
    }

    cmd.args(["--driver", "remote", &node.endpoint]);

    let output = cmd
        .output()
        .with_context(|| format!("failed to create shard builder {}", shard_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to create shard {}: {}", shard_name, stderr.trim());
    }

    Ok(shard_name)
}

/// Remove a shard builder. Best-effort, ignores errors.
pub fn remove_shard(shard_name: &str) {
    let _ = Command::new("docker")
        .args(["buildx", "rm", shard_name])
        .output();
}

/// Clean up shard builders left behind by runs that are no longer alive.
///
/// Shards belonging to a running `dbake` are left untouched — removing them
/// would break that run mid-build.
pub fn cleanup_stale_shards(builder_prefix: &str) -> Result<()> {
    let output = Command::new("docker")
        .args(["buildx", "ls"])
        .output()
        .context("failed to list builders")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for name in stale_shards(&stdout, builder_prefix, process_is_alive) {
        eprintln!("cleaning stale shard builder: {}", name);
        remove_shard(&name);
    }

    Ok(())
}

/// Pick the shard builders in `docker buildx ls` output that are safe to remove.
fn stale_shards(
    ls_output: &str,
    builder_prefix: &str,
    is_alive: impl Fn(u32) -> bool,
) -> Vec<String> {
    let mut stale = Vec::new();
    for line in ls_output.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        // Builder names appear with optional trailing * and are at the start of lines
        let name = name.trim_end_matches('*');
        match classify_shard(name, builder_prefix) {
            ShardKind::Foreign => {}
            ShardKind::Legacy => stale.push(name.to_string()),
            ShardKind::Owned(pid) if !is_alive(pid) => stale.push(name.to_string()),
            ShardKind::Owned(_) => {}
        }
    }
    stale
}

/// RAII guard that removes shard builders on drop.
pub struct ShardGuard {
    shard_names: Vec<String>,
}

impl ShardGuard {
    pub fn new() -> Self {
        Self {
            shard_names: Vec::new(),
        }
    }

    /// Register a shard for cleanup. Call this immediately after creating one:
    /// a shard that exists but is not registered leaks if a later create fails.
    pub fn push(&mut self, shard_name: String) {
        self.shard_names.push(shard_name);
    }
}

impl Default for ShardGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ShardGuard {
    fn drop(&mut self) {
        for name in &self.shard_names {
            remove_shard(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_names_are_unique_per_run() {
        assert_eq!(shard_name("zot", "node0", 42), "zot-shard-node0--run42");
        assert_ne!(
            shard_name("zot", "node0", 42),
            shard_name("zot", "node0", 43)
        );
    }

    #[test]
    fn classifies_names() {
        assert_eq!(
            classify_shard("zot-shard-node0--run42", "zot"),
            ShardKind::Owned(42)
        );
        assert_eq!(classify_shard("zot-shard-node0", "zot"), ShardKind::Legacy);
        assert_eq!(classify_shard("zot", "zot"), ShardKind::Foreign);
        assert_eq!(
            classify_shard("other-shard-n--run1", "zot"),
            ShardKind::Foreign
        );
    }

    #[test]
    fn node_names_containing_dashes_still_parse() {
        assert_eq!(
            classify_shard("zot-lb-shard-zot-m3-pro0--run991", "zot-lb"),
            ShardKind::Owned(991)
        );
    }

    #[test]
    fn a_legacy_shard_for_a_numeric_node_name_is_not_mistaken_for_a_pid() {
        // `zot-shard-worker-2` is a pre-run-id shard for node "worker-2".
        // Reading the trailing 2 as a pid would pin it to kthreadd, which is
        // always alive, so it would never be cleaned up.
        assert_eq!(
            classify_shard("zot-shard-worker-2", "zot"),
            ShardKind::Legacy
        );
        assert_eq!(
            stale_shards("zot-shard-worker-2  remote\n", "zot", |_| true),
            vec!["zot-shard-worker-2".to_string()]
        );
    }

    #[test]
    fn a_live_runs_shards_are_never_removed() {
        let ls = "NAME/NODE                 DRIVER/ENDPOINT\n\
                  zot-shard-node0--run100  remote\n\
                  zot-shard-node1--run200  remote\n\
                  zot                      remote\n";
        // pid 100 is still running, 200 is not.
        let stale = stale_shards(ls, "zot", |pid| pid == 100);
        assert_eq!(stale, vec!["zot-shard-node1--run200".to_string()]);
    }

    #[test]
    fn legacy_shards_without_a_run_id_are_removed() {
        let ls = "zot-shard-node0  remote\n";
        assert_eq!(
            stale_shards(ls, "zot", |_| true),
            vec!["zot-shard-node0".to_string()]
        );
    }

    #[test]
    fn other_builders_are_left_alone() {
        let ls = "default *  docker\nmy-cluster  remote\n";
        assert!(stale_shards(ls, "zot", |_| false).is_empty());
    }
}

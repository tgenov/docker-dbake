use anyhow::{Context, Result};
use std::process::Command;

use super::node::Node;

/// Create an ephemeral shard builder for a single node.
/// Returns the shard builder name.
pub fn create_shard(builder_prefix: &str, node: &Node, platform: Option<&str>) -> Result<String> {
    let shard_name = format!("{}-shard-{}", builder_prefix, node.name);

    // Remove stale shard if it exists
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

/// Clean up any stale shard builders from previous crashes.
pub fn cleanup_stale_shards(builder_prefix: &str) -> Result<()> {
    let output = Command::new("docker")
        .args(["buildx", "ls"])
        .output()
        .context("failed to list builders")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let shard_prefix = format!("{}-shard-", builder_prefix);

    for line in stdout.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        // Builder names appear with optional trailing * and are at the start of lines
        let name = name.trim_end_matches('*');
        if name.starts_with(&shard_prefix) {
            eprintln!("cleaning stale shard builder: {}", name);
            remove_shard(name);
        }
    }

    Ok(())
}

/// RAII guard that removes shard builders on drop.
pub struct ShardGuard {
    shard_names: Vec<String>,
}

impl ShardGuard {
    pub fn new(shard_names: Vec<String>) -> Self {
        Self { shard_names }
    }
}

impl Drop for ShardGuard {
    fn drop(&mut self) {
        for name in &self.shard_names {
            remove_shard(name);
        }
    }
}

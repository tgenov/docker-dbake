use anyhow::{Context, Result, bail};
use std::process::Command;

use super::node::Node;

/// Parse `docker buildx inspect <builder>` output into a list of nodes.
/// Only includes nodes with TCP endpoints that are in "running" status.
pub fn discover_nodes(builder: &str) -> Result<Vec<Node>> {
    let output = Command::new("docker")
        .args(["buildx", "inspect", builder])
        .output()
        .context("failed to run `docker buildx inspect`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker buildx inspect failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_inspect_output(&stdout)
}

fn parse_inspect_output(output: &str) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_endpoint: Option<String> = None;
    let mut current_status: Option<String> = None;
    let mut current_platforms: Vec<String> = Vec::new();

    let flush = |name: Option<String>,
                 endpoint: Option<String>,
                 status: Option<String>,
                 platforms: Vec<String>,
                 nodes: &mut Vec<Node>| {
        if let (Some(name), Some(endpoint)) = (name, endpoint) {
            let is_tcp = endpoint.starts_with("tcp://");
            let is_running = status.as_deref() == Some("running");
            if is_tcp && is_running {
                nodes.push(Node {
                    name,
                    endpoint,
                    platforms,
                });
            }
        }
    };

    for line in output.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("Name:") {
            flush(
                current_name.take(),
                current_endpoint.take(),
                current_status.take(),
                std::mem::take(&mut current_platforms),
                &mut nodes,
            );
            current_name = Some(rest.trim().to_string());
            current_endpoint = None;
            current_status = None;
            current_platforms.clear();
        } else if let Some(rest) = trimmed.strip_prefix("Endpoint:") {
            current_endpoint = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Status:") {
            current_status = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Platforms:") {
            current_platforms = rest
                .trim()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    // Flush last node
    flush(
        current_name,
        current_endpoint,
        current_status,
        current_platforms,
        &mut nodes,
    );

    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inspect_output() {
        let output = r#"
Name:          zot
Driver:        remote
Nodes:
Name:          zot0
Endpoint:      tcp://192.168.0.129:1234
Status:        running
Buildkit:      v0.12.4
Platforms:     linux/amd64, linux/arm64
Name:          zot-mini
Endpoint:      tcp://192.168.0.144:1234
Status:        running
Buildkit:      v0.12.4
Platforms:     linux/amd64
Name:          local-node
Endpoint:      unix:///var/run/buildkit/buildkitd.sock
Status:        running
Platforms:     linux/amd64
"#;
        let nodes = parse_inspect_output(output).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "zot0");
        assert_eq!(nodes[0].endpoint, "tcp://192.168.0.129:1234");
        assert_eq!(nodes[0].platforms, vec!["linux/amd64", "linux/arm64"]);
        assert_eq!(nodes[1].name, "zot-mini");
    }

    #[test]
    fn test_skips_inactive_nodes() {
        let output = r#"
Name:          zot-lb
Driver:        remote
Nodes:
Name:          zot-lb0
Endpoint:      tcp://192.168.0.129:1234
Status:        running
Platforms:     linux/arm64
Name:          zot-lb2
Endpoint:      tcp://192.168.0.141:1234
Status:        inactive
Name:          zot-lb3
Endpoint:      tcp://192.168.0.110:1235
Status:        running
Platforms:     linux/amd64
"#;
        let nodes = parse_inspect_output(output).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "zot-lb0");
        assert_eq!(nodes[1].name, "zot-lb3");
    }
}

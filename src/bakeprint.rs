use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;

/// A single target from `docker buildx bake --print` JSON output.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BakeTarget {
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
    #[serde(default, rename = "cache-from")]
    pub cache_from: Vec<CacheEntry>,
    #[serde(default, rename = "cache-to")]
    pub cache_to: Vec<CacheEntry>,
    #[serde(default, rename = "no-cache")]
    pub no_cache: Option<bool>,
    #[serde(default)]
    pub push: Option<bool>,
    #[serde(default)]
    pub load: Option<bool>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

/// Cache entry — can be a bare string or a structured type.
/// `docker buildx bake --print` serializes these as strings like
/// `"type=registry,ref=foo/bar"` or as objects.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CacheEntry {
    String(String),
    Object(serde_json::Value),
}

impl CacheEntry {
    pub fn to_arg(&self) -> String {
        match self {
            CacheEntry::String(s) => s.clone(),
            CacheEntry::Object(v) => v.to_string(),
        }
    }
}

/// Full bake print output with group and target sections.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BakePrint {
    #[serde(default)]
    pub group: HashMap<String, BakeGroup>,
    #[serde(default)]
    pub target: HashMap<String, BakeTarget>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BakeGroup {
    #[serde(default)]
    pub targets: Vec<String>,
}

/// Run `docker buildx bake --print` and parse the JSON output.
/// This is the canonical source of truth for targets — works for both
/// HCL bake files and docker-compose YAML.
pub fn bake_print(file: &str, builder: &str) -> Result<BakePrint> {
    let output = Command::new("docker")
        .args(["buildx", "bake", "--builder", builder, "-f", file, "--print"])
        .output()
        .context("failed to run `docker buildx bake --print`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker buildx bake --print failed: {}", stderr.trim());
    }

    let print: BakePrint = serde_json::from_slice(&output.stdout)
        .context("failed to parse bake --print JSON")?;

    Ok(print)
}

/// Extract depends_on edges from the bake file by re-parsing it ourselves.
/// `docker buildx bake --print` resolves targets but doesn't include depends_on
/// in its JSON output, so we parse the source file for dependency edges.
pub fn extract_depends_on(file: &str) -> Result<HashMap<String, Vec<String>>> {
    let content = std::fs::read_to_string(file)
        .context(format!("failed to read {}", file))?;

    // Try HCL-style parsing first (target blocks with depends_on)
    if file.ends_with(".hcl") || content.contains("target \"") {
        return parse_hcl_depends_on(&content);
    }

    // Fall back to YAML (docker-compose)
    parse_yaml_depends_on(&content)
}

/// Parse depends_on from HCL bake files.
/// Handles: `depends_on = ["target1", "target2"]`
fn parse_hcl_depends_on(content: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_target: Option<String> = None;
    let mut brace_depth: i32 = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        // Match `target "name" {`
        if let Some(rest) = trimmed.strip_prefix("target ") {
            if let Some(name) = extract_quoted_string(rest) {
                current_target = Some(name);
                if trimmed.ends_with('{') {
                    brace_depth = 1;
                }
                continue;
            }
        }

        if current_target.is_some() {
            brace_depth += trimmed.matches('{').count() as i32;
            brace_depth -= trimmed.matches('}').count() as i32;

            if brace_depth <= 0 {
                current_target = None;
                brace_depth = 0;
                continue;
            }

            // Match `depends_on = ["target1", "target2"]`
            if let Some(rest) = trimmed.strip_prefix("depends_on") {
                let rest = rest.trim().strip_prefix('=').unwrap_or(rest).trim();
                let dep_list = parse_hcl_string_list(rest);
                if let Some(ref target) = current_target {
                    deps.insert(target.clone(), dep_list);
                }
            }
        }
    }

    Ok(deps)
}

/// Parse depends_on from docker-compose YAML.
fn parse_yaml_depends_on(content: &str) -> Result<HashMap<String, Vec<String>>> {
    #[derive(Deserialize)]
    struct ComposeFile {
        #[serde(default)]
        services: HashMap<String, ServiceDeps>,
    }

    #[derive(Deserialize)]
    struct ServiceDeps {
        #[serde(default)]
        depends_on: DependsOn,
    }

    #[derive(Default, Deserialize)]
    #[serde(untagged)]
    enum DependsOn {
        #[default]
        None,
        List(Vec<String>),
        Map(HashMap<String, serde_json::Value>),
    }

    let compose: ComposeFile = serde_yaml::from_str(content)
        .context("failed to parse compose YAML for depends_on")?;

    let mut deps = HashMap::new();
    for (name, svc) in compose.services {
        let dep_list = match svc.depends_on {
            DependsOn::None => vec![],
            DependsOn::List(v) => v,
            DependsOn::Map(m) => m.into_keys().collect(),
        };
        if !dep_list.is_empty() {
            deps.insert(name, dep_list);
        }
    }

    Ok(deps)
}

fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim();
    if s.starts_with('"') {
        let end = s[1..].find('"')?;
        Some(s[1..=end].to_string())
    } else {
        None
    }
}

fn parse_hcl_string_list(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);
    s.split(',')
        .map(|item| {
            let item = item.trim();
            item.trim_matches('"').to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hcl_depends_on() {
        let hcl = r#"
target "base" {
  context = "."
  dockerfile = "Dockerfile.base"
}

target "app" {
  context = "."
  depends_on = ["base"]
}

target "tests" {
  context = "."
  depends_on = ["base", "app"]
}
"#;
        let deps = parse_hcl_depends_on(hcl).unwrap();
        assert_eq!(deps["app"], vec!["base"]);
        assert_eq!(deps["tests"], vec!["base", "app"]);
        assert!(!deps.contains_key("base"));
    }

    #[test]
    fn test_parse_yaml_depends_on_list() {
        let yaml = r#"
services:
  web:
    depends_on:
      - db
      - redis
  db:
    image: postgres
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert_eq!(deps["web"].len(), 2);
        assert!(!deps.contains_key("db"));
    }

    #[test]
    fn test_parse_yaml_depends_on_map() {
        let yaml = r#"
services:
  web:
    depends_on:
      db:
        condition: service_healthy
  db:
    image: postgres
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert_eq!(deps["web"], vec!["db"]);
    }

    #[test]
    fn test_parse_hcl_string_list() {
        assert_eq!(
            parse_hcl_string_list(r#"["foo", "bar", "baz"]"#),
            vec!["foo", "bar", "baz"]
        );
        assert_eq!(
            parse_hcl_string_list(r#"["single"]"#),
            vec!["single"]
        );
    }
}

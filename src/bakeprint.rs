use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;

/// A single target from `docker buildx bake --print` JSON output.
/// Fields deserialized from `bake --print` JSON. Not all fields are accessed directly —
/// they exist for serde structural parsing and potential future use.
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
/// Serde-only: `group` is parsed but not accessed in code.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BakePrint {
    #[serde(default)]
    pub group: HashMap<String, BakeGroup>,
    #[serde(default)]
    pub target: HashMap<String, BakeTarget>,
}

/// Serde-only: `targets` is parsed but not accessed in code.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BakeGroup {
    #[serde(default)]
    pub targets: Vec<String>,
}

/// Run `docker buildx bake --print` and parse the JSON output.
/// This is the canonical source of truth for targets — works for both
/// HCL bake files and docker-compose YAML.
pub fn bake_print(file: &std::path::Path, builder: &str) -> Result<BakePrint> {
    let output = Command::new("docker")
        .args([
            "buildx",
            "bake",
            "--builder",
            builder,
            "-f",
            &file.to_string_lossy(),
            "--print",
        ])
        .output()
        .context("failed to run `docker buildx bake --print`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker buildx bake --print failed: {}", stderr.trim());
    }

    let print: BakePrint =
        serde_json::from_slice(&output.stdout).context("failed to parse bake --print JSON")?;

    Ok(print)
}

/// Extract depends_on edges from the bake file by re-parsing it ourselves.
/// `docker buildx bake --print` resolves targets but doesn't include depends_on
/// in its JSON output, so we parse the source file for dependency edges.
pub fn extract_depends_on(file: &std::path::Path) -> Result<HashMap<String, Vec<String>>> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;

    // Try HCL-style parsing first (target blocks with depends_on)
    if file.extension().is_some_and(|e| e == "hcl") || content.contains("target \"") {
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

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Match `target "name" {`
        if let Some(rest) = trimmed.strip_prefix("target ") {
            if let Some(name) = extract_quoted_string(rest) {
                current_target = Some(name);
                // Count ALL braces on this line, not just a trailing '{'.
                brace_depth =
                    trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
                // If braces already balanced (e.g. `target "x" {}`), close immediately.
                if brace_depth <= 0 {
                    current_target = None;
                    brace_depth = 0;
                }
                i += 1;
                continue;
            }
        }

        if current_target.is_some() {
            brace_depth += trimmed.matches('{').count() as i32;
            brace_depth -= trimmed.matches('}').count() as i32;

            if brace_depth <= 0 {
                current_target = None;
                brace_depth = 0;
                i += 1;
                continue;
            }

            // Match `depends_on = ["target1", "target2"]` (single or multi-line)
            if let Some(rest) = trimmed.strip_prefix("depends_on") {
                let rest = rest.trim().strip_prefix('=').unwrap_or(rest).trim();
                // Strip inline comments after the value.
                let rest = strip_hcl_inline_comment(rest);

                let value = if rest.contains('[') && !rest.contains(']') {
                    // Multi-line array: accumulate until we find the closing bracket.
                    let mut accumulated = rest.to_string();
                    while i + 1 < lines.len() {
                        i += 1;
                        let next = strip_hcl_inline_comment(lines[i].trim());
                        accumulated.push_str(next);
                        if next.contains(']') {
                            break;
                        }
                    }
                    accumulated
                } else {
                    rest.to_string()
                };

                let dep_list = parse_hcl_string_list(&value);
                if let Some(ref target) = current_target {
                    deps.insert(target.clone(), dep_list);
                }
            }
        }

        i += 1;
    }

    Ok(deps)
}

/// Strip an inline comment (`//` or `#`) that appears outside of quoted strings.
fn strip_hcl_inline_comment(s: &str) -> &str {
    let mut in_quotes = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quotes = !in_quotes,
            b'/' if !in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return s[..i].trim_end();
            }
            b'#' if !in_quotes => {
                return s[..i].trim_end();
            }
            _ => {}
        }
        i += 1;
    }
    s
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
        depends_on: Option<DependsOn>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DependsOn {
        List(Vec<String>),
        Map(HashMap<String, serde_json::Value>),
    }

    let compose: ComposeFile =
        serde_yaml::from_str(content).context("failed to parse compose YAML for depends_on")?;

    let mut deps = HashMap::new();
    for (name, svc) in compose.services {
        let dep_list = match svc.depends_on {
            None => vec![],
            Some(DependsOn::List(v)) => v,
            Some(DependsOn::Map(m)) => m.into_keys().collect(),
        };
        if !dep_list.is_empty() {
            deps.insert(name, dep_list);
        }
    }

    Ok(deps)
}

fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix('"')?;
    // Walk the string to find the closing quote, skipping escaped quotes.
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Consume the escaped character and include it literally.
                if let Some(escaped) = chars.next() {
                    result.push(escaped);
                }
            }
            '"' => return Some(result),
            _ => result.push(ch),
        }
    }
    // No closing quote found.
    None
}

fn parse_hcl_string_list(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);

    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut prev_backslash = false;

    for ch in s.chars() {
        if prev_backslash {
            current.push(ch);
            prev_backslash = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => {
                prev_backslash = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                // Don't include the quote character in the value.
            }
            ',' if !in_quotes => {
                let val = current.trim().to_string();
                if !val.is_empty() {
                    items.push(val);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    let val = current.trim().to_string();
    if !val.is_empty() {
        items.push(val);
    }

    items
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
        assert_eq!(parse_hcl_string_list(r#"["single"]"#), vec!["single"]);
    }

    #[test]
    fn test_parse_hcl_string_list_with_commas_in_quotes() {
        // Bug 2: commas inside quoted strings should not split.
        assert_eq!(parse_hcl_string_list(r#"["a,b", "c"]"#), vec!["a,b", "c"]);
        assert_eq!(
            parse_hcl_string_list(r#"["one,two,three"]"#),
            vec!["one,two,three"]
        );
    }

    #[test]
    fn test_parse_hcl_depends_on_multiline() {
        // Bug 1: multi-line depends_on arrays.
        let hcl = r#"
target "app" {
  depends_on = [
    "base",
    "utils"
  ]
  context = "."
}
"#;
        let deps = parse_hcl_depends_on(hcl).unwrap();
        assert_eq!(deps["app"], vec!["base", "utils"]);
    }

    #[test]
    fn test_parse_hcl_single_line_empty_target() {
        // Bug 3: `target "x" {}` on one line should not consume later blocks.
        let hcl = r#"
target "empty" {}

target "real" {
  depends_on = ["base"]
}
"#;
        let deps = parse_hcl_depends_on(hcl).unwrap();
        assert!(!deps.contains_key("empty"));
        assert_eq!(deps["real"], vec!["base"]);
    }

    #[test]
    fn test_parse_hcl_inline_comments() {
        let hcl = r#"
target "app" {
  depends_on = ["base", "utils"] // build deps
}
"#;
        let deps = parse_hcl_depends_on(hcl).unwrap();
        assert_eq!(deps["app"], vec!["base", "utils"]);
    }

    #[test]
    fn test_parse_hcl_inline_hash_comment() {
        let hcl = r#"
target "app" {
  depends_on = ["base"] # comment
}
"#;
        let deps = parse_hcl_depends_on(hcl).unwrap();
        assert_eq!(deps["app"], vec!["base"]);
    }

    #[test]
    fn test_extract_quoted_string_escaped() {
        // Escaped quotes should not end the string early.
        assert_eq!(
            extract_quoted_string(r#""app\"name" {"#),
            Some(r#"app"name"#.to_string())
        );
    }

    #[test]
    fn test_extract_quoted_string_no_closing_quote() {
        assert_eq!(extract_quoted_string(r#""unclosed"#), None);
    }

    #[test]
    fn test_strip_hcl_inline_comment() {
        assert_eq!(strip_hcl_inline_comment(r#"["a"] // comment"#), r#"["a"]"#);
        assert_eq!(strip_hcl_inline_comment(r#"["a"] # comment"#), r#"["a"]"#);
        // Hash inside quotes should not be treated as a comment.
        assert_eq!(strip_hcl_inline_comment(r#""a#b""#), r#""a#b""#);
    }

    #[test]
    fn test_parse_yaml_depends_on_null() {
        let yaml = r#"
services:
  web:
    depends_on:
  db:
    image: postgres
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_yaml_depends_on_empty_list() {
        let yaml = r#"
services:
  web:
    depends_on: []
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_yaml_depends_on_empty_map() {
        let yaml = r#"
services:
  web:
    depends_on: {}
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_yaml_depends_on_multiple_conditions() {
        let yaml = r#"
services:
  web:
    depends_on:
      db:
        condition: service_healthy
      redis:
        condition: service_started
      cache:
        condition: service_completed_successfully
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert_eq!(deps["web"].len(), 3);
    }

    #[test]
    fn test_parse_yaml_with_anchors() {
        let yaml = r#"
services:
  base: &base
    build: .
  web:
    <<: *base
    depends_on:
      - db
  db:
    image: postgres
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert_eq!(deps["web"], vec!["db"]);
    }

    #[test]
    fn test_parse_yaml_service_no_depends_on() {
        let yaml = r#"
services:
  web:
    build: .
  db:
    image: postgres
"#;
        let deps = parse_yaml_depends_on(yaml).unwrap();
        assert!(deps.is_empty());
    }
}

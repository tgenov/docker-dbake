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
    /// Render the entry the way `docker buildx --set target.cache-from=` expects:
    /// comma-separated `key=value` pairs, `type` first.
    ///
    /// buildx serialises cache entries as JSON objects in `--print` output, so
    /// the object form must be flattened — passing the JSON through verbatim
    /// makes buildx fail with `bare " in non-quoted-field`.
    pub fn to_arg(&self) -> String {
        match self {
            CacheEntry::String(s) => s.clone(),
            CacheEntry::Object(v) => flatten_cache_object(v),
        }
    }
}

fn flatten_cache_object(v: &serde_json::Value) -> String {
    let Some(map) = v.as_object() else {
        // Not an object (buildx should never emit this) — fall back to the
        // scalar rendering, which is at least CSV-safe for strings and numbers.
        return scalar_to_string(v);
    };

    let mut parts = Vec::with_capacity(map.len());
    if let Some(ty) = map.get("type") {
        parts.push(format!("type={}", scalar_to_string(ty)));
    }
    for (k, val) in map {
        if k == "type" {
            continue;
        }
        parts.push(format!("{}={}", k, scalar_to_string(val)));
    }
    parts.join(",")
}

/// Render a JSON scalar without JSON quoting, which `--set` cannot parse.
fn scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
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
pub fn bake_print(file: &std::path::Path, builder: &str, targets: &[String]) -> Result<BakePrint> {
    bake_print_with(targets, |t| run_bake_print(file, builder, t), file)
}

/// Target-resolution policy, separated from the subprocess so it can be tested.
///
/// Without target arguments buildx resolves the implicit `default` group. That
/// group may be missing entirely (HCL files need not define one) or may cover
/// only some targets, so a target-less print is not sufficient on its own.
fn bake_print_with(
    targets: &[String],
    mut run: impl FnMut(&[String]) -> Result<BakePrint>,
    file: &std::path::Path,
) -> Result<BakePrint> {
    let full = run(&[]);

    match full {
        // The default group covers everything asked for.
        Ok(print) if covers(&print, targets) => Ok(print),
        // Either the default group is missing, or it does not include every
        // requested target. Ask buildx for exactly what we want.
        _ if !targets.is_empty() => {
            let retry = run(targets);
            match (retry, full) {
                (Ok(print), _) => Ok(print),
                (Err(retry_err), Err(first)) => {
                    Err(retry_err.context(format!("target-less print also failed: {:#}", first)))
                }
                // The retry failed but the default group resolved fine. Usually
                // that means one of the names is not a buildable target — a
                // typo, or a compose service with no `build:` pulled in by
                // depends_on — so keep the usable result and let target
                // selection produce a good message. Surface the discarded error
                // regardless: the retry can also fail for unrelated reasons
                // (registry auth, a bad context) that the user needs to see.
                (Err(retry_err), Ok(print)) => {
                    eprintln!(
                        "warning: buildx could not resolve [{}] directly ({:#}); \
                         continuing with the targets it did resolve",
                        targets.join(", "),
                        retry_err
                    );
                    Ok(print)
                }
            }
        }
        // No targets requested and the default group is missing entirely.
        Err(first) => Err(first.context(no_default_group_hint(file))),
        // Unreachable: `covers` is vacuously true for an empty target list, so
        // arm A already matched. Kept for exhaustiveness.
        Ok(print) => Ok(print),
    }
}

/// Whether a print result contains every requested target.
fn covers(print: &BakePrint, targets: &[String]) -> bool {
    targets.iter().all(|t| print.target.contains_key(t))
}

/// Guidance for the most common reason a target-less `--print` fails.
fn no_default_group_hint(file: &std::path::Path) -> String {
    format!(
        "{} defines no `default` group and no targets were given.\n\
         Either add `group \"default\" {{ targets = [...] }}` to the file, or name \
         the targets to build: docker dbake -f {} <target>...",
        file.display(),
        file.display()
    )
}

fn run_bake_print(file: &std::path::Path, builder: &str, targets: &[String]) -> Result<BakePrint> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "buildx",
        "bake",
        "--builder",
        builder,
        "-f",
        &file.to_string_lossy(),
        "--print",
    ]);
    cmd.args(targets);

    let output = cmd
        .output()
        .context("failed to run `docker buildx bake --print`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker buildx bake --print failed: {}", stderr.trim());
    }

    serde_json::from_slice(&output.stdout).context("failed to parse bake --print JSON")
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
        serde_yaml_ng::from_str(content).context("failed to parse compose YAML for depends_on")?;

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

    fn print_with(names: &[&str]) -> BakePrint {
        let targets: String = names
            .iter()
            .map(|n| format!("\"{}\": {{}}", n))
            .collect::<Vec<_>>()
            .join(",");
        serde_json::from_str(&format!("{{\"target\": {{{}}}}}", targets)).unwrap()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn uses_the_default_group_when_it_covers_everything() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let out = bake_print_with(
            &names(&["app"]),
            |t| {
                calls.push(t.to_vec());
                Ok(print_with(&["app", "other"]))
            },
            std::path::Path::new("x.hcl"),
        )
        .unwrap();
        assert_eq!(out.target.len(), 2);
        assert_eq!(
            calls,
            vec![Vec::<String>::new()],
            "must not retry needlessly"
        );
    }

    #[test]
    fn retries_with_targets_when_there_is_no_default_group() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let out = bake_print_with(
            &names(&["app"]),
            |t| {
                calls.push(t.to_vec());
                if t.is_empty() {
                    bail!("failed to find target default")
                }
                Ok(print_with(&["app"]))
            },
            std::path::Path::new("x.hcl"),
        )
        .unwrap();
        assert!(out.target.contains_key("app"));
        assert_eq!(calls, vec![vec![], names(&["app"])]);
    }

    #[test]
    fn retries_when_the_default_group_is_only_partial() {
        // `group "default" { targets = ["app"] }` plus a separate target
        // "extra": the target-less print returns only `app`, so asking for
        // `extra` must not be reported as an unknown target.
        let mut calls: Vec<Vec<String>> = Vec::new();
        let out = bake_print_with(
            &names(&["extra"]),
            |t| {
                calls.push(t.to_vec());
                if t.is_empty() {
                    Ok(print_with(&["app"]))
                } else {
                    Ok(print_with(&["extra"]))
                }
            },
            std::path::Path::new("x.hcl"),
        )
        .unwrap();
        assert!(out.target.contains_key("extra"));
        assert_eq!(calls.len(), 2, "partial default group must trigger a retry");
    }

    #[test]
    fn an_unknown_target_falls_back_to_the_usable_full_print() {
        // buildx cannot resolve the name, but the default group did resolve.
        // Returning that lets target selection produce "unknown target 'nope' /
        // did you mean ...", which is far more useful than buildx's message.
        let out = bake_print_with(
            &names(&["nope"]),
            |t| {
                if t.is_empty() {
                    Ok(print_with(&["app"]))
                } else {
                    bail!("failed to find target nope")
                }
            },
            std::path::Path::new("x.hcl"),
        )
        .unwrap();
        assert_eq!(out.target.len(), 1);
        assert!(out.target.contains_key("app"));
    }

    #[test]
    fn a_non_buildable_compose_dependency_does_not_abort_the_run() {
        // `app depends_on db` where db has no build section: --with-deps adds
        // `db` to the wanted list, buildx rejects it, and the run must still
        // build `app` rather than dying.
        let out = bake_print_with(
            &names(&["app", "db"]),
            |t| {
                if t.is_empty() {
                    Ok(print_with(&["app"]))
                } else {
                    bail!("failed to find target db")
                }
            },
            std::path::Path::new("docker-compose.yml"),
        )
        .unwrap();
        assert!(out.target.contains_key("app"));
    }

    #[test]
    fn both_prints_failing_reports_both_errors() {
        let err = bake_print_with(
            &names(&["app"]),
            |_| bail!("boom"),
            std::path::Path::new("x.hcl"),
        )
        .unwrap_err();
        assert!(format!("{:#}", err).contains("target-less print also failed"));
    }

    #[test]
    fn no_targets_and_no_default_group_explains_how_to_recover() {
        let err = bake_print_with(
            &[],
            |_| bail!("failed to find target default"),
            std::path::Path::new("docker-bake.hcl"),
        )
        .unwrap_err();
        assert!(
            format!("{:#}", err).contains("no `default` group"),
            "{err:#}"
        );
    }

    #[test]
    fn hint_explains_how_to_recover_from_a_missing_default_group() {
        let hint = no_default_group_hint(std::path::Path::new("docker-bake.hcl"));
        assert!(hint.contains("no `default` group"), "{hint}");
        assert!(
            hint.contains("docker dbake -f docker-bake.hcl <target>"),
            "{hint}"
        );
    }

    #[test]
    fn cache_entry_string_is_passed_through() {
        let e = CacheEntry::String("type=registry,ref=foo/bar".into());
        assert_eq!(e.to_arg(), "type=registry,ref=foo/bar");
    }

    #[test]
    fn cache_entry_object_is_flattened_to_csv_with_type_first() {
        // buildx >= 0.17 emits cache entries as objects; --set wants CSV k=v.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"src":"/tmp/c1","type":"local"}"#).unwrap();
        assert_eq!(CacheEntry::Object(v).to_arg(), "type=local,src=/tmp/c1");
    }

    #[test]
    fn cache_entry_object_handles_non_string_values() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"type":"registry","ref":"r/foo","oci-mediatypes":true}"#)
                .unwrap();
        let arg = CacheEntry::Object(v).to_arg();
        assert!(arg.starts_with("type=registry"), "{arg}");
        assert!(arg.contains("oci-mediatypes=true"), "{arg}");
        assert!(!arg.contains('"'), "no JSON quoting may survive: {arg}");
    }

    #[test]
    fn cache_entry_object_without_type_still_produces_csv() {
        let v: serde_json::Value = serde_json::from_str(r#"{"ref":"r/foo"}"#).unwrap();
        assert_eq!(CacheEntry::Object(v).to_arg(), "ref=r/foo");
    }

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

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Result};

/// Everything needed to decide which targets to build.
///
/// Extracted from `main` so the filtering pipeline is testable without a
/// docker daemon: profile gating, explicit target selection, `--with-deps`
/// expansion and `--exclude` are all pure functions of this input.
pub struct SelectionInput<'a> {
    /// Every target buildx resolved from the bake file, sorted.
    pub all_targets: &'a [String],
    /// target → targets it depends on (may name targets outside `all_targets`).
    pub deps: &'a HashMap<String, Vec<String>>,
    /// service → declared compose profiles. `None` for bake files without profiles.
    pub profiles: Option<&'a HashMap<String, Vec<String>>>,
    /// The `--profile` values, if any. A service is active when it declares
    /// any of them (or declares none at all).
    pub profiles_wanted: &'a [String],
    /// Explicitly requested targets (positional args).
    pub requested: &'a [String],
    /// `--exclude` values.
    pub exclude: &'a [String],
    /// Whether to expand the dependency chain of the requested targets.
    pub with_deps: bool,
    /// Whether `deps` describes build ordering (HCL) rather than runtime
    /// startup ordering (compose `depends_on`). Warnings about excluding a
    /// dependency only make sense for build dependencies.
    pub deps_are_build_deps: bool,
}

/// Whether a bake file is HCL, where `depends_on` is a *build* dependency.
///
/// The extension is authoritative when present; otherwise fall back to looking
/// for an HCL `target "name" {` block header. Matching the bare string
/// `target "` would also fire on a compose file that merely mentions it — in a
/// comment, an image tag or a build arg — so require the block-opening brace.
pub fn is_hcl_bake_file(path: &std::path::Path, contents: Option<&str>) -> bool {
    if let Some(ext) = path.extension() {
        if ext == "hcl" {
            return true;
        }
        if ext == "yml" || ext == "yaml" {
            return false;
        }
        if ext == "json" {
            // `docker-bake.json` is a supported bake format; treating it as
            // compose would silently discard its build ordering.
            return contents.is_some_and(is_json_bake_file);
        }
    }

    contents.is_some_and(|c| {
        let mut lines = c.lines().peekable();
        while let Some(line) = lines.next() {
            // HCL target blocks are top level, so a matching line has no
            // indentation. That also rules out an HCL snippet quoted inside a
            // YAML block scalar, which is always indented.
            if line.starts_with("target \"") {
                // The brace may be on this line or the next.
                if line.contains('{') {
                    return true;
                }
                if lines
                    .peek()
                    .is_some_and(|next| next.trim_start().starts_with('{'))
                {
                    return true;
                }
            }
        }
        false
    })
}

/// Whether JSON content is a bake file (has a top-level `target` object).
fn is_json_bake_file(contents: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(contents)
        .ok()
        .and_then(|v| v.get("target").map(|t| t.is_object()))
        .unwrap_or(false)
}

/// Transitively expand `targets` through `deps`.
///
/// Used before asking buildx to print, because `bake --print <target>` reports
/// only that target — dependencies would otherwise never enter the target set.
pub fn expand_deps(targets: &[String], deps: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut expanded: HashSet<String> = targets.iter().cloned().collect();
    let mut queue: VecDeque<String> = targets.iter().cloned().collect();

    while let Some(t) = queue.pop_front() {
        let Some(dep_list) = deps.get(&t) else {
            continue;
        };
        for dep in dep_list {
            if expanded.insert(dep.clone()) {
                queue.push_back(dep.clone());
            }
        }
    }

    let mut out: Vec<String> = expanded.into_iter().collect();
    out.sort();
    out
}

/// The selected targets plus any non-fatal warnings the caller should print.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SelectionOutcome {
    pub targets: Vec<String>,
    pub warnings: Vec<String>,
}

/// Resolve the set of targets to build.
///
/// Profile gating follows compose semantics: a service with no `profiles:` key
/// is always active, and `--profile` *adds* the services gated behind it.
/// Unknown target names are rejected rather than silently dropped.
pub fn select_targets(input: SelectionInput) -> Result<SelectionOutcome> {
    let known: HashSet<&str> = input.all_targets.iter().map(String::as_str).collect();
    let mut warnings = Vec::new();

    let mut targets: Vec<String> = input.all_targets.to_vec();

    if !input.profiles_wanted.is_empty() {
        let profiles = input.profiles.unwrap_or(&EMPTY_PROFILES);
        for wanted in input.profiles_wanted {
            if !profiles.values().any(|p| p.iter().any(|x| x == wanted)) {
                bail!(
                    "no service declares profile '{}'{}",
                    wanted,
                    available_profiles(profiles)
                );
            }
        }
        // Compose semantics: profile-less services are always active, and each
        // --profile adds the services gated behind it.
        targets.retain(|t| {
            profiles.get(t).is_none_or(|declared| {
                declared.is_empty()
                    || declared
                        .iter()
                        .any(|d| input.profiles_wanted.iter().any(|w| w == d))
            })
        });
    }

    if !input.requested.is_empty() {
        let unknown: Vec<&String> = input
            .requested
            .iter()
            .filter(|t| !known.contains(t.as_str()))
            .collect();
        if let Some(first) = unknown.first() {
            bail!(
                "unknown target '{}'\n  available: {}{}",
                first,
                input.all_targets.join(", "),
                suggestion(first, input.all_targets)
            );
        }

        // A requested target that exists but was gated out by --profile is a
        // conflict, not a silent no-op.
        let selected: HashSet<&str> = targets.iter().map(String::as_str).collect();
        let gated: Vec<&str> = input
            .requested
            .iter()
            .map(String::as_str)
            .filter(|t| !selected.contains(t))
            .collect();
        if !gated.is_empty() {
            bail!(
                "target(s) {} are not in profile(s) {}",
                gated.join(", "),
                input.profiles_wanted.join(", ")
            );
        }

        let requested: HashSet<&str> = input.requested.iter().map(String::as_str).collect();
        targets.retain(|t| requested.contains(t.as_str()));
    }

    if input.with_deps {
        let mut expanded: HashSet<String> = targets.iter().cloned().collect();
        let mut queue: VecDeque<String> = targets.iter().cloned().collect();

        while let Some(t) = queue.pop_front() {
            let Some(dep_list) = input.deps.get(&t) else {
                continue;
            };
            for dep in dep_list {
                if known.contains(dep.as_str()) && expanded.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }

        targets = expanded.into_iter().collect();
        targets.sort();
    }

    if !input.exclude.is_empty() {
        let exclude_set: HashSet<&str> = input.exclude.iter().map(String::as_str).collect();

        for name in input.exclude {
            if !known.contains(name.as_str()) {
                warnings.push(format!("--exclude '{}' matches no target", name));
            }
        }

        // Dropping a target that a kept target depends on removes the edge
        // entirely (DagQueue only links deps inside the target set), so the
        // dependent builds against whatever stale image exists on the node.
        let kept: Vec<&String> = if input.deps_are_build_deps {
            targets
                .iter()
                .filter(|t| !exclude_set.contains(t.as_str()))
                .collect()
        } else {
            Vec::new()
        };
        for target in &kept {
            let Some(dep_list) = input.deps.get(*target) else {
                continue;
            };
            for dep in dep_list {
                if exclude_set.contains(dep.as_str()) {
                    warnings.push(format!(
                        "'{}' depends on excluded target '{}' — it will build against \
                         whatever '{}' image already exists on the node",
                        target, dep, dep
                    ));
                }
            }
        }

        targets.retain(|t| !exclude_set.contains(t.as_str()));
    }

    if targets.is_empty() {
        bail!("no targets to build after filtering");
    }

    Ok(SelectionOutcome { targets, warnings })
}

static EMPTY_PROFILES: std::sync::LazyLock<HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(HashMap::new);

fn available_profiles(profiles: &HashMap<String, Vec<String>>) -> String {
    let mut all: Vec<&str> = profiles
        .values()
        .flat_map(|p| p.iter().map(String::as_str))
        .collect();
    all.sort_unstable();
    all.dedup();
    if all.is_empty() {
        String::new()
    } else {
        format!("\n  available profiles: {}", all.join(", "))
    }
}

/// Suggest the closest known target name, when one is close enough to be a typo.
fn suggestion(name: &str, candidates: &[String]) -> String {
    let best = candidates
        .iter()
        .map(|c| (levenshtein(name, c), c))
        .min_by_key(|(d, _)| *d);

    match best {
        // Allow roughly one edit per three characters before calling it a typo.
        Some((dist, candidate)) if dist <= (name.len() / 3).max(1) => {
            format!("\n  did you mean '{}'?", candidate)
        }
        _ => String::new(),
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0usize; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn input<'a>(all: &'a [String]) -> SelectionInput<'a> {
        SelectionInput {
            all_targets: all,
            deps: &EMPTY_DEPS,
            profiles: None,
            profiles_wanted: &[],
            requested: &[],
            exclude: &[],
            with_deps: false,
            deps_are_build_deps: true,
        }
    }

    static EMPTY_DEPS: std::sync::LazyLock<HashMap<String, Vec<String>>> =
        std::sync::LazyLock::new(HashMap::new);

    static FRONTEND: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["frontend".to_string()]);
    static TYPO: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["frontnd".to_string()]);

    #[test]
    fn selects_everything_by_default() {
        let all = names(&["api", "web"]);
        let out = select_targets(input(&all)).unwrap();
        assert_eq!(out.targets, names(&["api", "web"]));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn errors_when_no_targets_at_all() {
        let all: Vec<String> = vec![];
        let err = select_targets(input(&all)).unwrap_err();
        assert!(err.to_string().contains("no targets to build"));
    }

    // --- #8: compose profile semantics ---

    #[test]
    fn profile_keeps_services_without_profiles() {
        // docker compose --profile frontend builds `api` (ungated) AND `web`.
        let all = names(&["api", "web"]);
        let profiles = HashMap::from([
            ("web".to_string(), names(&["frontend"])),
            ("api".to_string(), vec![]),
        ]);
        let out = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &FRONTEND,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api", "web"]));
    }

    #[test]
    fn profile_excludes_services_gated_behind_another_profile() {
        let all = names(&["api", "batch", "web"]);
        let profiles = HashMap::from([
            ("web".to_string(), names(&["frontend"])),
            ("batch".to_string(), names(&["jobs"])),
            ("api".to_string(), vec![]),
        ]);
        let out = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &FRONTEND,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api", "web"]));
    }

    #[test]
    fn unknown_profile_is_an_error_naming_the_valid_ones() {
        let all = names(&["web"]);
        let profiles = HashMap::from([("web".to_string(), names(&["frontend"]))]);
        let err = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &TYPO,
            ..input(&all)
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("no service declares profile 'frontnd'"),
            "{err}"
        );
        assert!(err.contains("frontend"), "{err}");
    }

    // --- #14: unknown target names ---

    #[test]
    fn unknown_requested_target_is_an_error() {
        let all = names(&["api", "web", "worker"]);
        let requested = names(&["web", "wroker"]);
        let err = select_targets(SelectionInput {
            requested: &requested,
            ..input(&all)
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown target 'wroker'"), "{err}");
        assert!(err.contains("available: api, web, worker"), "{err}");
        assert!(err.contains("did you mean 'worker'?"), "{err}");
    }

    #[test]
    fn known_requested_targets_are_selected() {
        let all = names(&["api", "web", "worker"]);
        let requested = names(&["web", "api"]);
        let out = select_targets(SelectionInput {
            requested: &requested,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api", "web"]));
    }

    #[test]
    fn wildly_different_name_gets_no_suggestion() {
        let all = names(&["api"]);
        let requested = names(&["zzzzzzzzzz"]);
        let err = select_targets(SelectionInput {
            requested: &requested,
            ..input(&all)
        })
        .unwrap_err()
        .to_string();
        assert!(!err.contains("did you mean"), "{err}");
    }

    #[test]
    fn requesting_a_target_gated_out_by_profile_names_the_conflict() {
        // `batch` exists and `frontend` is a real profile, so neither the
        // unknown-target nor the unknown-profile check fires: this pins the
        // gating branch specifically.
        let all = names(&["api", "batch", "web"]);
        let profiles = HashMap::from([
            ("web".to_string(), names(&["frontend"])),
            ("batch".to_string(), names(&["jobs"])),
            ("api".to_string(), vec![]),
        ]);
        let requested = names(&["batch"]);
        let err = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &FRONTEND,
            requested: &requested,
            ..input(&all)
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("target(s) batch are not in profile(s) frontend"),
            "{err}"
        );
    }

    #[test]
    fn multiple_profiles_are_additive() {
        let all = names(&["api", "batch", "web"]);
        let profiles = HashMap::from([
            ("web".to_string(), names(&["frontend"])),
            ("batch".to_string(), names(&["jobs"])),
            ("api".to_string(), vec![]),
        ]);
        let wanted = names(&["frontend", "jobs"]);
        let out = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &wanted,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api", "batch", "web"]));
    }

    #[test]
    fn a_target_absent_from_the_profile_map_is_always_active() {
        // HCL targets have no compose profiles entry at all.
        let all = names(&["hcl-only", "web"]);
        let profiles = HashMap::from([("web".to_string(), names(&["frontend"]))]);
        let out = select_targets(SelectionInput {
            profiles: Some(&profiles),
            profiles_wanted: &FRONTEND,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["hcl-only", "web"]));
    }

    #[test]
    fn compose_deps_do_not_produce_build_dependency_warnings() {
        // compose depends_on is runtime ordering: excluding `db` says nothing
        // about how `web` builds.
        let all = names(&["db", "web"]);
        let deps = HashMap::from([("web".to_string(), names(&["db"]))]);
        let exclude = names(&["db"]);
        let out = select_targets(SelectionInput {
            deps: &deps,
            exclude: &exclude,
            deps_are_build_deps: false,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["web"]));
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn expand_deps_is_transitive_and_terminates_on_cycles() {
        let deps = HashMap::from([
            ("app".to_string(), names(&["base"])),
            ("base".to_string(), names(&["utils"])),
            ("utils".to_string(), names(&["app"])),
        ]);
        assert_eq!(
            expand_deps(&names(&["app"]), &deps),
            names(&["app", "base", "utils"])
        );
        assert_eq!(expand_deps(&[], &deps), Vec::<String>::new());
    }

    #[test]
    fn expand_deps_keeps_dependencies_the_bake_file_has_not_printed_yet() {
        // This runs BEFORE `bake --print`, so it must not filter by known
        // targets — that is exactly what made --with-deps a silent no-op.
        let deps = HashMap::from([("app".to_string(), names(&["base"]))]);
        assert_eq!(
            expand_deps(&names(&["app"]), &deps),
            names(&["app", "base"])
        );
    }

    // --- --with-deps ---

    #[test]
    fn with_deps_expands_transitively() {
        let all = names(&["app", "base", "utils", "unrelated"]);
        let deps = HashMap::from([
            ("app".to_string(), names(&["base"])),
            ("base".to_string(), names(&["utils"])),
        ]);
        let requested = names(&["app"]);
        let out = select_targets(SelectionInput {
            deps: &deps,
            requested: &requested,
            with_deps: true,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["app", "base", "utils"]));
    }

    #[test]
    fn with_deps_ignores_deps_outside_the_bake_file() {
        let all = names(&["app"]);
        let deps = HashMap::from([("app".to_string(), names(&["external"]))]);
        let requested = names(&["app"]);
        let out = select_targets(SelectionInput {
            deps: &deps,
            requested: &requested,
            with_deps: true,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["app"]));
    }

    #[test]
    fn with_deps_terminates_on_a_dependency_cycle() {
        let all = names(&["a", "b"]);
        let deps = HashMap::from([
            ("a".to_string(), names(&["b"])),
            ("b".to_string(), names(&["a"])),
        ]);
        let requested = names(&["a"]);
        let out = select_targets(SelectionInput {
            deps: &deps,
            requested: &requested,
            with_deps: true,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["a", "b"]));
    }

    // --- --exclude ---

    #[test]
    fn exclude_removes_targets() {
        let all = names(&["api", "web"]);
        let exclude = names(&["web"]);
        let out = select_targets(SelectionInput {
            exclude: &exclude,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api"]));
    }

    #[test]
    fn excluding_an_unknown_target_warns() {
        let all = names(&["api"]);
        let exclude = names(&["nope"]);
        let out = select_targets(SelectionInput {
            exclude: &exclude,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["api"]));
        assert!(
            out.warnings.iter().any(|w| w.contains("matches no target")),
            "{:?}",
            out.warnings
        );
    }

    #[test]
    fn excluding_a_dependency_of_a_kept_target_warns() {
        let all = names(&["app", "base"]);
        let deps = HashMap::from([("app".to_string(), names(&["base"]))]);
        let exclude = names(&["base"]);
        let out = select_targets(SelectionInput {
            deps: &deps,
            exclude: &exclude,
            ..input(&all)
        })
        .unwrap();
        assert_eq!(out.targets, names(&["app"]));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("depends on excluded")),
            "{:?}",
            out.warnings
        );
    }

    #[test]
    fn excluding_everything_is_an_error() {
        let all = names(&["api"]);
        let exclude = names(&["api"]);
        let err = select_targets(SelectionInput {
            exclude: &exclude,
            ..input(&all)
        })
        .unwrap_err();
        assert!(err.to_string().contains("no targets to build"));
    }

    #[test]
    fn hcl_detection_uses_the_extension_first() {
        use std::path::Path;
        assert!(is_hcl_bake_file(Path::new("docker-bake.hcl"), None));
        assert!(!is_hcl_bake_file(
            Path::new("docker-compose.yml"),
            Some("target \"x\" {")
        ));
        assert!(!is_hcl_bake_file(Path::new("compose.yaml"), None));
    }

    #[test]
    fn hcl_detection_falls_back_to_a_block_header() {
        use std::path::Path;
        assert!(is_hcl_bake_file(
            Path::new("bakefile"),
            Some("target \"app\" {\n  context = \".\"\n}")
        ));
    }

    #[test]
    fn a_json_bake_file_is_hcl_not_compose() {
        use std::path::Path;
        let bake = r#"{"target": {"app": {"context": "."}}}"#;
        assert!(is_hcl_bake_file(Path::new("docker-bake.json"), Some(bake)));

        let compose = r#"{"services": {"web": {"build": "."}}}"#;
        assert!(!is_hcl_bake_file(Path::new("compose.json"), Some(compose)));
    }

    #[test]
    fn hcl_detection_handles_a_brace_on_the_next_line() {
        use std::path::Path;
        assert!(is_hcl_bake_file(
            Path::new("bakefile"),
            Some("target \"app\"\n{\n  context = \".\"\n}")
        ));
    }

    #[test]
    fn an_hcl_snippet_quoted_inside_compose_yaml_is_not_hcl() {
        use std::path::Path;
        let compose = "services:\n  web:\n    build: .\nx-notes: |\n  target \"app\" { }\n";
        assert!(!is_hcl_bake_file(Path::new("stack"), Some(compose)));
    }

    #[test]
    fn hcl_detection_handles_a_single_line_block() {
        use std::path::Path;
        assert!(is_hcl_bake_file(
            Path::new("bakefile"),
            Some("target \"empty\" {}\n")
        ));
        assert!(is_hcl_bake_file(
            Path::new("bakefile"),
            Some("target \"x\" { context = \".\" }\n")
        ));
    }

    #[test]
    fn a_compose_file_merely_mentioning_target_is_not_hcl() {
        use std::path::Path;
        // The old check was `contents.contains("target \"")`, which fired here.
        let compose = "services:\n  web:\n    build:\n      target: \"builder\"\n";
        assert!(!is_hcl_bake_file(Path::new("stack"), Some(compose)));
        let commented = "# see target \"app\" in the old bakefile\nservices:\n  web: {}\n";
        assert!(!is_hcl_bake_file(Path::new("stack"), Some(commented)));
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("worker", "worker"), 0);
        assert_eq!(levenshtein("wroker", "worker"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }
}

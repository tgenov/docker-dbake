use std::collections::{HashMap, HashSet, VecDeque};

/// A dependency-aware, platform-aware work queue. Targets become ready
/// only when all their dependencies have completed, and can only be
/// claimed by nodes that support the target's required platform(s).
pub struct DagQueue {
    /// Forward edges: target → set of targets it depends on
    deps: HashMap<String, HashSet<String>>,
    /// Reverse edges: target → set of targets that depend on it
    rdeps: HashMap<String, HashSet<String>>,
    /// Targets whose deps are all satisfied, available to claim
    ready: VecDeque<String>,
    /// Currently being built
    in_flight: HashSet<String>,
    /// Successfully completed
    completed: HashSet<String>,
    /// Failed
    failed: HashSet<String>,
    /// Cancelled — terminal, but NOT a failure, so dependents are not told a
    /// dependency failed when the user simply pressed Ctrl+C.
    cancelled: HashSet<String>,
    /// All target names
    all: HashSet<String>,
    /// Per-target required platforms (empty = any node can build it)
    target_platforms: HashMap<String, Vec<String>>,
    /// Platform lists of the nodes that will run builds. Empty when unknown,
    /// in which case every ready target is assumed claimable.
    node_platforms: Vec<Vec<String>>,
}

/// Normalize a platform string for comparison.
/// "linux/amd64", "linux/amd64/v2" etc. We compare the base arch.
pub fn platform_base(p: &str) -> &str {
    // "linux/amd64/v2" → "linux/amd64", "linux/arm64" → "linux/arm64"
    match p.find('/') {
        Some(first_slash) => {
            let rest = &p[first_slash + 1..];
            match rest.find('/') {
                Some(second_slash) => &p[..first_slash + 1 + second_slash],
                None => p, // only "os/arch", no variant
            }
        }
        None => p,
    }
}

/// Whether a requested platform is satisfied by one a node reports.
///
/// When both sides name a variant they must match exactly — `linux/arm/v7` is
/// not `linux/arm/v6`. When either side omits the variant it is treated as
/// unspecific, so `linux/arm64` is satisfied by `linux/arm64/v8`.
pub fn platform_matches(wanted: &str, available: &str) -> bool {
    let has_variant = |p: &str| p.split('/').count() > 2;
    if has_variant(wanted) && has_variant(available) {
        return wanted == available;
    }
    platform_base(wanted) == platform_base(available)
}

/// Check if a node's platform list can build a target's required platforms.
/// A node is compatible if it supports at least one of the target's platforms.
fn platforms_compatible(node_platforms: &[String], target_platforms: &[String]) -> bool {
    if target_platforms.is_empty() {
        return true; // No platform constraint — any node works
    }

    target_platforms
        .iter()
        .any(|tp| node_platforms.iter().any(|np| platform_matches(tp, np)))
}

impl DagQueue {
    /// Build a DAG queue from targets, dependency edges, and per-target platforms.
    /// `deps` maps target → list of targets it depends on.
    /// `target_platforms` maps target → list of required platforms.
    /// Only dependencies that are themselves in `targets` are considered.
    pub fn new(
        targets: Vec<String>,
        deps: HashMap<String, Vec<String>>,
        target_platforms: HashMap<String, Vec<String>>,
    ) -> Self {
        let all: HashSet<String> = targets.iter().cloned().collect();

        // Build forward and reverse edge maps, filtering to only known targets
        let mut forward: HashMap<String, HashSet<String>> = HashMap::new();
        let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();

        for t in &targets {
            forward.entry(t.clone()).or_default();
            reverse.entry(t.clone()).or_default();
        }

        for (target, dep_list) in &deps {
            if !all.contains(target) {
                continue;
            }
            for dep in dep_list {
                if dep == target {
                    eprintln!(
                        "warning: target '{}' depends on itself — ignoring self-dependency",
                        target
                    );
                    continue;
                }
                if all.contains(dep) {
                    forward
                        .entry(target.clone())
                        .or_default()
                        .insert(dep.clone());
                    reverse
                        .entry(dep.clone())
                        .or_default()
                        .insert(target.clone());
                }
            }
        }

        // Seed the ready queue with targets that have no (in-set) dependencies
        let mut ready = VecDeque::new();
        for t in &targets {
            if forward.get(t).is_none_or(|d| d.is_empty()) {
                ready.push_back(t.clone());
            }
        }

        Self {
            deps: forward,
            rdeps: reverse,
            ready,
            in_flight: HashSet::new(),
            completed: HashSet::new(),
            failed: HashSet::new(),
            cancelled: HashSet::new(),
            all,
            target_platforms,
            node_platforms: Vec::new(),
        }
    }

    /// Tell the queue which nodes exist, so it can tell "waiting for a build"
    /// apart from "no node can ever claim what is left".
    pub fn with_nodes(mut self, node_platforms: Vec<Vec<String>>) -> Self {
        self.node_platforms = node_platforms;
        self
    }

    /// Targets whose platform constraints no known node can satisfy.
    ///
    /// Returned as (target, required platforms) so the caller can explain the
    /// problem instead of parking forever waiting for a build that cannot start.
    pub fn unsatisfiable_targets(&self) -> Vec<(String, Vec<String>)> {
        if self.node_platforms.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<(String, Vec<String>)> = self
            .all
            .iter()
            .filter_map(|t| {
                let required = self.target_platforms.get(t)?;
                if required.is_empty() || self.claimable_by_any_node(t) {
                    None
                } else {
                    Some((t.clone(), required.clone()))
                }
            })
            .collect();
        out.sort();
        out
    }

    fn claimable_by_any_node(&self, target: &str) -> bool {
        if self.node_platforms.is_empty() {
            return true; // nodes unknown — assume claimable
        }
        let required = self
            .target_platforms
            .get(target)
            .cloned()
            .unwrap_or_default();
        self.node_platforms
            .iter()
            .any(|np| platforms_compatible(np, &required))
    }

    /// Every target that has not reached a terminal state.
    ///
    /// Used on shutdown so cancelled and blocked targets are reported rather
    /// than left `Pending`, which would stall any consumer waiting for the run
    /// to complete.
    pub fn unfinished_targets(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .all
            .iter()
            .filter(|t| {
                !self.completed.contains(*t)
                    && !self.failed.contains(*t)
                    && !self.cancelled.contains(*t)
            })
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Claim the next ready target that is compatible with the given node platforms.
    /// Returns None if no compatible target is currently available.
    pub fn claim_for_platforms(&mut self, node_platforms: &[String]) -> Option<String> {
        // Find the first ready target compatible with this node
        let pos = self.ready.iter().position(|t| {
            let tp = self.target_platforms.get(t).cloned().unwrap_or_default();
            platforms_compatible(node_platforms, &tp)
        })?;

        let target = self.ready.remove(pos).unwrap();
        self.in_flight.insert(target.clone());
        Some(target)
    }

    /// Mark a target as successfully completed. This may unblock dependent
    /// targets, adding them to the ready queue.
    pub fn complete(&mut self, target: &str) {
        self.in_flight.remove(target);
        self.completed.insert(target.to_string());
        self.release_dependents(target);
    }

    /// Mark a target as failed. Dependents will never become ready.
    pub fn fail(&mut self, target: &str) {
        self.in_flight.remove(target);
        self.failed.insert(target.to_string());
    }

    /// Mark a target as cancelled: terminal, but not a failure.
    ///
    /// Kept separate from `fail` so a dependent is not told its dependency
    /// failed when the run was merely interrupted.
    pub fn cancel(&mut self, target: &str) {
        self.in_flight.remove(target);
        self.cancelled.insert(target.to_string());
    }

    /// Check if all targets are either completed or failed.
    pub fn is_done(&self) -> bool {
        self.completed.len() + self.failed.len() + self.cancelled.len() == self.all.len()
    }

    /// Check if no further progress is possible.
    ///
    /// A ready target that no node can claim counts as stalled: otherwise every
    /// worker parks waiting for a completion notification that can never come.
    pub fn is_stalled(&self) -> bool {
        if self.is_done() || !self.in_flight.is_empty() {
            return false;
        }
        if self.ready.is_empty() {
            return true;
        }
        !self.ready.iter().any(|t| self.claimable_by_any_node(t))
    }

    /// Get the required platforms for a target (empty if unconstrained).
    pub fn target_platforms(&self, target: &str) -> &[String] {
        self.target_platforms
            .get(target)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Number of targets ready to be claimed.
    #[cfg(test)]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Dependencies of `target` that failed, transitively.
    ///
    /// Distinguishes "blocked by a failed dependency" from "never started",
    /// which the user needs in order to know where to look.
    pub fn failed_dependencies(&self, target: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![target.to_string()];

        while let Some(t) = stack.pop() {
            let Some(deps) = self.deps.get(&t) else {
                continue;
            };
            for dep in deps {
                if !seen.insert(dep.clone()) {
                    continue;
                }
                if self.failed.contains(dep) {
                    out.push(dep.clone());
                } else if !self.completed.contains(dep) && !self.cancelled.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }

        out.sort();
        out.dedup();
        out
    }

    /// Detect cycles in the dependency graph using DFS.
    /// Returns a list of cycles, where each cycle is a list of target names.
    /// An empty return means the graph is acyclic.
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        let mut visited = HashSet::new();
        let mut on_stack = HashSet::new();
        let mut stack = Vec::new();
        let mut cycles = Vec::new();

        for target in &self.all {
            if !visited.contains(target) {
                self.dfs_cycle(target, &mut visited, &mut on_stack, &mut stack, &mut cycles);
            }
        }

        cycles
    }

    fn dfs_cycle(
        &self,
        node: &str,
        visited: &mut HashSet<String>,
        on_stack: &mut HashSet<String>,
        stack: &mut Vec<String>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        visited.insert(node.to_string());
        on_stack.insert(node.to_string());
        stack.push(node.to_string());

        if let Some(deps) = self.deps.get(node) {
            for dep in deps {
                if !visited.contains(dep) {
                    self.dfs_cycle(dep, visited, on_stack, stack, cycles);
                } else if on_stack.contains(dep) {
                    // Found a cycle — extract it from the stack
                    if let Some(pos) = stack.iter().position(|s| s == dep) {
                        let cycle: Vec<String> = stack[pos..].to_vec();
                        cycles.push(cycle);
                    }
                }
            }
        }

        stack.pop();
        on_stack.remove(node);
    }

    /// After completing a target, check all its dependents to see if they
    /// are now unblocked.
    fn release_dependents(&mut self, completed_target: &str) {
        let dependents = match self.rdeps.get(completed_target) {
            Some(d) => d.clone(),
            None => return,
        };

        for dep in dependents {
            if self.completed.contains(&dep)
                || self.failed.contains(&dep)
                || self.in_flight.contains(&dep)
                || self.ready.iter().any(|r| r == &dep)
            {
                continue;
            }

            let all_deps_done = self
                .deps
                .get(&dep)
                .is_none_or(|d| d.iter().all(|dd| self.completed.contains(dd)));

            if all_deps_done {
                self.ready.push_back(dep);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_platforms() -> HashMap<String, Vec<String>> {
        HashMap::new()
    }

    #[test]
    fn test_independent_targets() {
        let targets = vec!["a".into(), "b".into(), "c".into()];
        let mut q = DagQueue::new(targets, HashMap::new(), no_platforms());

        assert_eq!(q.ready_count(), 3);
        let all = vec!["linux/amd64".into()];
        assert!(q.claim_for_platforms(&all).is_some());
        assert!(q.claim_for_platforms(&all).is_some());
        assert!(q.claim_for_platforms(&all).is_some());
        assert!(q.claim_for_platforms(&all).is_none());
    }

    #[test]
    fn test_linear_chain() {
        let targets = vec!["a".into(), "b".into(), "c".into()];
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["b".to_string()]),
        ]);
        let mut q = DagQueue::new(targets, deps, no_platforms());
        let any = vec!["linux/amd64".into()];

        assert_eq!(q.ready_count(), 1);
        let t = q.claim_for_platforms(&any).unwrap();
        assert_eq!(t, "a");

        q.complete("a");
        assert_eq!(q.ready_count(), 1);
        let t = q.claim_for_platforms(&any).unwrap();
        assert_eq!(t, "b");

        q.complete("b");
        let t = q.claim_for_platforms(&any).unwrap();
        assert_eq!(t, "c");

        q.complete("c");
        assert!(q.is_done());
    }

    #[test]
    fn test_diamond_dag() {
        let targets = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["a".to_string()]),
            ("d".to_string(), vec!["b".to_string(), "c".to_string()]),
        ]);
        let mut q = DagQueue::new(targets, deps, no_platforms());
        let any = vec!["linux/amd64".into()];

        assert_eq!(q.ready_count(), 1);
        q.claim_for_platforms(&any).unwrap(); // a
        q.complete("a");
        assert_eq!(q.ready_count(), 2);

        let t1 = q.claim_for_platforms(&any).unwrap();
        let t2 = q.claim_for_platforms(&any).unwrap();
        assert!(q.claim_for_platforms(&any).is_none());

        q.complete(&t1);
        assert_eq!(q.ready_count(), 0);
        q.complete(&t2);
        assert_eq!(q.ready_count(), 1);

        q.claim_for_platforms(&any).unwrap(); // d
        q.complete("d");
        assert!(q.is_done());
    }

    #[test]
    fn test_failure_blocks_dependents() {
        let targets = vec!["a".into(), "b".into(), "c".into()];
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["b".to_string()]),
        ]);
        let mut q = DagQueue::new(targets, deps, no_platforms());
        let any = vec!["linux/amd64".into()];

        q.claim_for_platforms(&any).unwrap(); // a
        q.fail("a");
        assert_eq!(q.ready_count(), 0);
        assert!(q.is_stalled());
        assert_eq!(
            q.unfinished_targets(),
            vec!["b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn test_deps_outside_target_set_ignored() {
        let targets = vec!["a".into(), "b".into()];
        let deps = HashMap::from([("b".to_string(), vec!["external".to_string()])]);
        let q = DagQueue::new(targets, deps, no_platforms());
        assert_eq!(q.ready_count(), 2);
    }

    #[test]
    fn test_platform_filtering() {
        let targets = vec!["arm-app".into(), "amd-app".into(), "any-app".into()];
        let platforms = HashMap::from([
            ("arm-app".to_string(), vec!["linux/arm64".to_string()]),
            ("amd-app".to_string(), vec!["linux/amd64".to_string()]),
            // any-app has no platform constraint
        ]);
        let mut q = DagQueue::new(targets, HashMap::new(), platforms);

        let arm_node = vec!["linux/arm64".into()];
        let amd_node = vec!["linux/amd64".into()];

        // ARM node should only get arm-app and any-app
        let t = q.claim_for_platforms(&arm_node).unwrap();
        assert_eq!(t, "arm-app");

        // AMD node should get amd-app
        let t = q.claim_for_platforms(&amd_node).unwrap();
        assert_eq!(t, "amd-app");

        // any-app is claimable by either
        let t = q.claim_for_platforms(&arm_node).unwrap();
        assert_eq!(t, "any-app");

        assert!(q.claim_for_platforms(&arm_node).is_none());
        assert!(q.claim_for_platforms(&amd_node).is_none());
    }

    #[test]
    fn test_platform_variant_matching() {
        // Node supports linux/amd64/v2, target wants linux/amd64
        let targets = vec!["app".into()];
        let platforms = HashMap::from([("app".to_string(), vec!["linux/amd64".to_string()])]);
        let mut q = DagQueue::new(targets, HashMap::new(), platforms);

        let node = vec!["linux/amd64/v2".into()];
        let t = q.claim_for_platforms(&node);
        assert!(t.is_some());
    }

    #[test]
    fn test_no_compatible_node_skips() {
        let targets = vec!["arm-only".into()];
        let platforms = HashMap::from([("arm-only".to_string(), vec!["linux/arm64".to_string()])]);
        let mut q = DagQueue::new(targets, HashMap::new(), platforms);

        let amd_node = vec!["linux/amd64".into()];
        assert!(q.claim_for_platforms(&amd_node).is_none());
        // Target is still ready, just not for this node
        assert_eq!(q.ready_count(), 1);
    }

    #[test]
    fn test_empty_targets() {
        let q = DagQueue::new(vec![], HashMap::new(), no_platforms());
        assert!(q.is_done());
        assert!(!q.is_stalled());
    }

    #[test]
    fn test_single_target() {
        let mut q = DagQueue::new(vec!["solo".into()], HashMap::new(), no_platforms());
        let any = vec!["linux/amd64".into()];
        let t = q.claim_for_platforms(&any).unwrap();
        assert_eq!(t, "solo");
        q.complete("solo");
        assert!(q.is_done());
    }

    #[test]
    fn test_self_dependency_filtered() {
        let deps = HashMap::from([("a".to_string(), vec!["a".to_string()])]);
        let mut q = DagQueue::new(vec!["a".into()], deps, no_platforms());
        // Self-dep should be filtered -- target should be ready
        let any = vec!["linux/amd64".into()];
        assert!(q.claim_for_platforms(&any).is_some());
    }

    #[test]
    fn test_cycle_detection() {
        let deps = HashMap::from([
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
        ]);
        let q = DagQueue::new(vec!["a".into(), "b".into()], deps, no_platforms());
        let cycles = q.detect_cycles();
        assert!(!cycles.is_empty());
    }

    #[test]
    fn test_diamond_partial_failure() {
        // A -> B, A -> C, B+C -> D. If B fails, D should stay blocked even after C completes.
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["a".to_string()]),
            ("d".to_string(), vec!["b".to_string(), "c".to_string()]),
        ]);
        let mut q = DagQueue::new(
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            deps,
            no_platforms(),
        );
        let any = vec!["linux/amd64".into()];

        q.claim_for_platforms(&any); // a
        q.complete("a");

        q.claim_for_platforms(&any); // b
        q.claim_for_platforms(&any); // c

        q.fail("b");
        q.complete("c");

        // d should NOT be ready (b failed)
        assert!(q.claim_for_platforms(&any).is_none());
        assert!(!q.is_done());
        assert!(q.unfinished_targets().contains(&"d".to_string()));
        assert_eq!(q.failed_dependencies("d"), vec!["b".to_string()]);
    }

    #[test]
    fn test_platforms_both_empty() {
        // No platform constraint, no node platforms -- should still be compatible
        assert!(super::platforms_compatible(&[], &[]));
    }

    #[test]
    fn test_node_platforms_empty_target_constrained() {
        // Node has no platforms but target requires one -- incompatible
        assert!(!super::platforms_compatible(
            &[],
            &["linux/amd64".to_string()]
        ));
    }

    #[test]
    fn explicit_variants_must_match_exactly() {
        // Asking for arm/v7 and being handed an arm/v6 builder is a silent
        // substitution of something the user explicitly did not ask for.
        assert!(!super::platform_matches("linux/arm/v7", "linux/arm/v6"));
        assert!(super::platform_matches("linux/arm/v7", "linux/arm/v7"));
    }

    #[test]
    fn an_unspecified_variant_matches_any() {
        assert!(super::platform_matches("linux/arm64", "linux/arm64/v8"));
        assert!(super::platform_matches("linux/amd64/v3", "linux/amd64"));
        assert!(super::platform_matches("linux/amd64", "linux/amd64"));
        assert!(!super::platform_matches("linux/amd64", "linux/arm64"));
    }

    #[test]
    fn test_platform_base_edge_cases() {
        assert_eq!(super::platform_base(""), "");
        assert_eq!(super::platform_base("linux"), "linux");
        assert_eq!(super::platform_base("linux/amd64"), "linux/amd64");
        assert_eq!(super::platform_base("linux/amd64/v2"), "linux/amd64");
        assert_eq!(super::platform_base("linux/arm/v7/extra"), "linux/arm");
        assert_eq!(super::platform_base("/amd64"), "/amd64");
    }

    #[test]
    fn unclaimable_target_is_stalled_not_a_deadlock() {
        // #2: an amd-only fleet with an arm-only target used to leave every
        // worker parked on notified.await forever.
        let targets = vec!["a".into(), "arm_only".into()];
        let platforms = HashMap::from([("arm_only".to_string(), vec!["linux/arm64".to_string()])]);
        let mut q = DagQueue::new(targets, HashMap::new(), platforms)
            .with_nodes(vec![vec!["linux/amd64".to_string()]]);

        let amd = vec!["linux/amd64".to_string()];
        let t = q.claim_for_platforms(&amd).unwrap();
        q.complete(&t);

        assert!(q.claim_for_platforms(&amd).is_none());
        assert!(!q.is_done());
        assert!(q.is_stalled(), "worker must be able to exit its loop");
    }

    #[test]
    fn unsatisfiable_targets_are_reported_with_their_platforms() {
        let targets = vec!["a".into(), "arm_only".into()];
        let platforms = HashMap::from([("arm_only".to_string(), vec!["linux/arm64".to_string()])]);
        let q = DagQueue::new(targets, HashMap::new(), platforms)
            .with_nodes(vec![vec!["linux/amd64".to_string()]]);

        assert_eq!(
            q.unsatisfiable_targets(),
            vec![("arm_only".to_string(), vec!["linux/arm64".to_string()])]
        );
    }

    #[test]
    fn multi_arch_node_satisfies_everything() {
        let targets = vec!["arm".into(), "amd".into()];
        let platforms = HashMap::from([
            ("arm".to_string(), vec!["linux/arm64".to_string()]),
            ("amd".to_string(), vec!["linux/amd64".to_string()]),
        ]);
        let q = DagQueue::new(targets, HashMap::new(), platforms).with_nodes(vec![vec![
            "linux/amd64".to_string(),
            "linux/arm64".to_string(),
        ]]);
        assert!(q.unsatisfiable_targets().is_empty());
        assert!(!q.is_stalled());
    }

    #[test]
    fn without_node_information_nothing_is_unsatisfiable() {
        // Absent node data we must not guess: a target we cannot evaluate is
        // never reported unbuildable, and the queue never declares itself
        // stalled on it (which is what would resurrect the original hang).
        let targets = vec!["arm".into(), "amd".into()];
        let platforms = HashMap::from([
            ("arm".to_string(), vec!["linux/arm64".to_string()]),
            ("amd".to_string(), vec!["linux/amd64".to_string()]),
        ]);
        let q = DagQueue::new(targets, HashMap::new(), platforms);

        assert!(q.unsatisfiable_targets().is_empty());
        assert!(!q.is_stalled(), "ready work must not look stalled");
        // And with node data, the same fixture DOES report one.
        let q = q.with_nodes(vec![vec!["linux/amd64".to_string()]]);
        assert_eq!(
            q.unsatisfiable_targets(),
            vec![("arm".to_string(), vec!["linux/arm64".to_string()])]
        );
    }

    #[test]
    fn in_flight_work_is_never_stalled() {
        let mut q = DagQueue::new(vec!["a".into(), "b".into()], HashMap::new(), no_platforms())
            .with_nodes(vec![vec!["linux/amd64".to_string()]]);
        let amd = vec!["linux/amd64".to_string()];
        q.claim_for_platforms(&amd).unwrap();
        q.claim_for_platforms(&amd).unwrap();
        assert!(!q.is_stalled());
    }

    #[test]
    fn unfinished_targets_covers_pending_and_blocked() {
        // #4: after cancellation, un-started targets must still be reportable.
        let deps = HashMap::from([("b".to_string(), vec!["a".to_string()])]);
        let mut q = DagQueue::new(
            vec!["a".into(), "b".into(), "c".into()],
            deps,
            no_platforms(),
        );
        let amd = vec!["linux/amd64".to_string()];
        let t = q.claim_for_platforms(&amd).unwrap();
        q.complete(&t);

        let unfinished = q.unfinished_targets();
        assert!(unfinished.contains(&"b".to_string()));
        assert!(unfinished.contains(&"c".to_string()));
        assert!(!unfinished.contains(&"a".to_string()));
    }

    #[test]
    fn unfinished_targets_is_empty_when_everything_settled() {
        let mut q = DagQueue::new(vec!["a".into(), "b".into()], HashMap::new(), no_platforms());
        q.complete("a");
        q.fail("b");
        assert!(q.unfinished_targets().is_empty());
        assert!(q.is_done());
    }

    #[test]
    fn failed_dependencies_are_reported_transitively() {
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["b".to_string()]),
        ]);
        let mut q = DagQueue::new(
            vec!["a".into(), "b".into(), "c".into()],
            deps,
            no_platforms(),
        );
        let amd = vec!["linux/amd64".to_string()];
        q.claim_for_platforms(&amd);
        q.fail("a");

        assert_eq!(q.failed_dependencies("b"), vec!["a".to_string()]);
        // c is blocked by b, which is itself blocked by the failed a.
        assert_eq!(q.failed_dependencies("c"), vec!["a".to_string()]);
    }

    #[test]
    fn a_cancelled_dependency_is_not_reported_as_failed() {
        // Ctrl+C must not tell the user a dependency FAILED.
        let deps = HashMap::from([
            ("b".to_string(), vec!["a".to_string()]),
            ("d".to_string(), vec!["c".to_string()]),
        ]);
        let mut q = DagQueue::new(
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            deps,
            no_platforms(),
        );
        let amd = vec!["linux/amd64".to_string()];
        q.claim_for_platforms(&amd);
        q.claim_for_platforms(&amd);
        q.cancel("a");
        q.fail("c");

        assert!(
            q.failed_dependencies("b").is_empty(),
            "a cancelled dependency is not a failure"
        );
        assert_eq!(q.failed_dependencies("d"), vec!["c".to_string()]);
    }

    #[test]
    fn cancelled_targets_are_terminal() {
        let mut q = DagQueue::new(vec!["a".into(), "b".into()], HashMap::new(), no_platforms());
        q.complete("a");
        assert!(!q.is_done());
        q.cancel("b");
        assert!(q.is_done(), "cancelled targets must not keep the run open");
        assert!(q.unfinished_targets().is_empty());
    }

    #[test]
    fn test_wide_fanout() {
        // One root, 50 dependents
        let mut targets = vec!["root".to_string()];
        let mut deps = HashMap::new();
        for i in 0..50 {
            let name = format!("child_{}", i);
            deps.insert(name.clone(), vec!["root".to_string()]);
            targets.push(name);
        }
        let mut q = DagQueue::new(targets, deps, no_platforms());
        let any = vec!["linux/amd64".into()];

        assert_eq!(q.ready_count(), 1);
        q.claim_for_platforms(&any); // root
        q.complete("root");
        assert_eq!(q.ready_count(), 50);
    }
}

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
    /// All target names
    all: HashSet<String>,
    /// Per-target required platforms (empty = any node can build it)
    target_platforms: HashMap<String, Vec<String>>,
}

/// Normalize a platform string for comparison.
/// "linux/amd64", "linux/amd64/v2" etc. We compare the base arch.
fn platform_base(p: &str) -> &str {
    // "linux/amd64/v2" → "linux/amd64", "linux/arm64" → "linux/arm64"
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() >= 2 {
        // Return "os/arch" without variant
        let end = p.find('/').unwrap() + 1 + parts[1].len();
        &p[..end]
    } else {
        p
    }
}

/// Check if a node's platform list can build a target's required platforms.
/// A node is compatible if it supports at least one of the target's platforms.
fn platforms_compatible(node_platforms: &[String], target_platforms: &[String]) -> bool {
    if target_platforms.is_empty() {
        return true; // No platform constraint — any node works
    }

    let node_bases: HashSet<&str> = node_platforms.iter().map(|p| platform_base(p)).collect();

    target_platforms
        .iter()
        .any(|tp| node_bases.contains(platform_base(tp)))
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
            all,
            target_platforms,
        }
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

    /// Check if all targets are either completed or failed.
    pub fn is_done(&self) -> bool {
        self.completed.len() + self.failed.len() == self.all.len()
    }

    /// Check if there's no more work possible (nothing ready, nothing in flight).
    pub fn is_stalled(&self) -> bool {
        self.ready.is_empty() && self.in_flight.is_empty() && !self.is_done()
    }

    /// Number of targets ready to be claimed.
    #[cfg(test)]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Targets that are blocked because a dependency failed.
    pub fn blocked_targets(&self) -> Vec<String> {
        self.all
            .iter()
            .filter(|t| {
                !self.completed.contains(*t)
                    && !self.failed.contains(*t)
                    && !self.in_flight.contains(*t)
                    && !self.ready.iter().any(|r| r == *t)
            })
            .cloned()
            .collect()
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
        assert_eq!(q.blocked_targets().len(), 2);
    }

    #[test]
    fn test_deps_outside_target_set_ignored() {
        let targets = vec!["a".into(), "b".into()];
        let deps = HashMap::from([("b".to_string(), vec!["external".to_string()])]);
        let mut q = DagQueue::new(targets, deps, no_platforms());
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
}

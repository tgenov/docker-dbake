use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum TargetStatus {
    Pending,
    Building,
    Done(Duration),
    Failed(String),
    /// Never started because the run was cancelled.
    Cancelled,
    /// Never started because a dependency failed. Carries the failed deps.
    Blocked(Vec<String>),
}

impl TargetStatus {
    /// Whether the target has reached a state it will not leave.
    ///
    /// Anything not terminal keeps the run from being considered complete, so
    /// cancelled and blocked targets must count here — otherwise consumers
    /// waiting for completion wait forever.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TargetStatus::Done(_)
                | TargetStatus::Failed(_)
                | TargetStatus::Cancelled
                | TargetStatus::Blocked(_)
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuildProgress {
    /// Current user-visible step (e.g. 3 in [3/5])
    pub current_step: u32,
    /// Total user-visible steps (e.g. 5 in [3/5])
    pub total_steps: u32,
    /// Description of the current step (e.g. "RUN npm install")
    pub step_description: String,
    /// Build stage the step belongs to, when BuildKit names one (e.g. "builder").
    pub stage: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub name: String,
    pub node: String,
    pub status: TargetStatus,
    pub started_at: Option<Instant>,
    pub log_path: Option<PathBuf>,
    pub progress: Option<BuildProgress>,
}

#[derive(Debug)]
pub struct DashboardState {
    pub targets: HashMap<String, TargetInfo>,
    pub target_order: Vec<String>,
    pub node_names: Vec<String>,
    pub total: usize,
    pub start_time: Instant,
    /// Bumped on every mutation so the TUI can skip redundant redraws.
    version: u64,
}

impl DashboardState {
    pub fn new(targets: Vec<String>, node_names: Vec<String>) -> Self {
        let total = targets.len();
        let mut target_map = HashMap::new();
        for t in &targets {
            target_map.insert(
                t.clone(),
                TargetInfo {
                    name: t.clone(),
                    node: String::new(),
                    status: TargetStatus::Pending,
                    started_at: None,
                    log_path: None,
                    progress: None,
                },
            );
        }
        Self {
            targets: target_map,
            target_order: targets,
            node_names,
            total,
            start_time: Instant::now(),
            version: 0,
        }
    }

    /// Monotonic counter of state changes.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Record where a target's build log is being written.
    ///
    /// Goes through a method so the redraw counter cannot be bypassed by
    /// mutating `targets` directly.
    pub fn set_log_path(&mut self, target: &str, path: PathBuf) {
        self.version += 1;
        if let Some(info) = self.targets.get_mut(target) {
            info.log_path = Some(path);
        }
    }

    pub fn set_target_status(&mut self, target: &str, node: &str, status: TargetStatus) {
        self.version += 1;
        if let Some(info) = self.targets.get_mut(target) {
            info.node = node.to_string();
            if matches!(status, TargetStatus::Building) {
                info.started_at = Some(Instant::now());
                info.progress = None;
            }
            info.status = status;
        }
    }

    pub fn update_progress(&mut self, target: &str, progress: BuildProgress) {
        self.version += 1;
        if let Some(info) = self.targets.get_mut(target) {
            info.progress = Some(progress);
        }
    }

    pub fn count_done(&self) -> usize {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Done(_)))
            .count()
    }

    pub fn count_building(&self) -> usize {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Building))
            .count()
    }

    pub fn count_pending(&self) -> usize {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Pending))
            .count()
    }

    pub fn count_failed(&self) -> usize {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Failed(_)))
            .count()
    }

    pub fn count_skipped(&self) -> usize {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Cancelled | TargetStatus::Blocked(_)))
            .count()
    }

    pub fn count_terminal(&self) -> usize {
        self.targets
            .values()
            .filter(|t| t.status.is_terminal())
            .count()
    }

    /// Targets not rendered under any known node — either never assigned one
    /// (cancelled, blocked) or assigned a node absent from `node_names`.
    /// Rendered separately so they can never be counted but not shown.
    pub fn unassigned_targets(&self) -> Vec<&TargetInfo> {
        let mut out: Vec<&TargetInfo> = self
            .targets
            .values()
            .filter(|t| {
                !matches!(t.status, TargetStatus::Pending) && !self.node_names.contains(&t.node)
            })
            .collect();
        out.sort_by_key(|t| &t.name);
        out
    }

    pub fn targets_for_node(&self, node: &str) -> Vec<&TargetInfo> {
        let mut targets: Vec<_> = self.targets.values().filter(|t| t.node == node).collect();
        targets.sort_by_key(|t| &t.name);
        targets
    }

    pub fn pending_targets(&self) -> Vec<&str> {
        self.target_order
            .iter()
            .filter(|t| {
                self.targets
                    .get(*t)
                    .map(|info| matches!(info.status, TargetStatus::Pending))
                    .unwrap_or(false)
            })
            .map(|s| s.as_str())
            .collect()
    }

    pub fn failed_targets(&self) -> Vec<&TargetInfo> {
        self.targets
            .values()
            .filter(|t| matches!(t.status, TargetStatus::Failed(_)))
            .collect()
    }

    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    pub fn is_complete(&self) -> bool {
        self.count_terminal() == self.total
    }

    /// All non-pending targets in on-screen order: grouped by node, sorted
    /// within each node. Matches the render order in the TUI dashboard so
    /// j/k navigation follows visual layout.
    pub fn active_targets(&self) -> Vec<&str> {
        let mut result = Vec::new();
        for node_name in &self.node_names {
            let mut node_targets: Vec<&str> = self
                .targets
                .values()
                .filter(|t| t.node == *node_name && !matches!(t.status, TargetStatus::Pending))
                .map(|t| t.name.as_str())
                .collect();
            node_targets.sort();
            result.extend(node_targets);
        }
        // Cancelled/blocked targets have no node; without this they would be
        // counted in the footer but unreachable by the cursor.
        result.extend(self.unassigned_targets().iter().map(|t| t.name.as_str()));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(targets: Vec<&str>, nodes: Vec<&str>) -> DashboardState {
        DashboardState::new(
            targets.into_iter().map(String::from).collect(),
            nodes.into_iter().map(String::from).collect(),
        )
    }

    #[test]
    fn test_new_empty() {
        let s = make_state(vec![], vec![]);
        assert_eq!(s.total, 0);
        assert!(s.is_complete());
        assert_eq!(s.count_done(), 0);
        assert_eq!(s.count_building(), 0);
        assert_eq!(s.count_pending(), 0);
        assert_eq!(s.count_failed(), 0);
    }

    #[test]
    fn test_new_initializes_all_pending() {
        let s = make_state(vec!["a", "b", "c"], vec!["node1"]);
        assert_eq!(s.total, 3);
        assert_eq!(s.count_pending(), 3);
        assert_eq!(s.count_building(), 0);
        assert_eq!(s.count_done(), 0);
        assert!(!s.is_complete());
    }

    #[test]
    fn test_status_transitions() {
        let mut s = make_state(vec!["a"], vec!["node1"]);

        // Pending -> Building
        s.set_target_status("a", "node1", TargetStatus::Building);
        assert_eq!(s.count_building(), 1);
        assert_eq!(s.count_pending(), 0);
        assert!(s.targets["a"].started_at.is_some());

        // Building -> Done
        s.set_target_status("a", "node1", TargetStatus::Done(Duration::from_secs(5)));
        assert_eq!(s.count_done(), 1);
        assert_eq!(s.count_building(), 0);
        assert!(s.is_complete());
    }

    #[test]
    fn test_status_building_to_failed() {
        let mut s = make_state(vec!["a"], vec!["node1"]);
        s.set_target_status("a", "node1", TargetStatus::Building);
        s.set_target_status("a", "node1", TargetStatus::Failed("error".into()));
        assert_eq!(s.count_failed(), 1);
        assert_eq!(s.count_building(), 0);
        assert!(s.is_complete());
    }

    #[test]
    fn test_set_status_nonexistent_target() {
        let mut s = make_state(vec!["a"], vec!["node1"]);
        // Should be a no-op, not panic
        s.set_target_status("nonexistent", "node1", TargetStatus::Building);
        assert_eq!(s.count_building(), 0);
        assert_eq!(s.count_pending(), 1);
    }

    #[test]
    fn test_targets_for_node() {
        let mut s = make_state(vec!["a", "b", "c"], vec!["n1", "n2"]);
        s.set_target_status("a", "n1", TargetStatus::Building);
        s.set_target_status("b", "n2", TargetStatus::Building);
        s.set_target_status("c", "n1", TargetStatus::Done(Duration::from_secs(1)));

        let n1_targets = s.targets_for_node("n1");
        assert_eq!(n1_targets.len(), 2);
        // Should be sorted by name
        assert_eq!(n1_targets[0].name, "a");
        assert_eq!(n1_targets[1].name, "c");

        let n2_targets = s.targets_for_node("n2");
        assert_eq!(n2_targets.len(), 1);
        assert_eq!(n2_targets[0].name, "b");
    }

    #[test]
    fn test_targets_for_unknown_node() {
        let s = make_state(vec!["a"], vec!["n1"]);
        assert!(s.targets_for_node("unknown").is_empty());
    }

    #[test]
    fn test_pending_targets_preserves_order() {
        let mut s = make_state(vec!["z", "a", "m"], vec!["n1"]);
        // Only z and m remain pending
        s.set_target_status("a", "n1", TargetStatus::Building);
        let pending = s.pending_targets();
        // Should preserve target_order (insertion order), not alphabetical
        assert_eq!(pending, vec!["z", "m"]);
    }

    #[test]
    fn test_failed_targets() {
        let mut s = make_state(vec!["a", "b", "c"], vec!["n1"]);
        s.set_target_status("a", "n1", TargetStatus::Failed("err1".into()));
        s.set_target_status("b", "n1", TargetStatus::Done(Duration::from_secs(1)));
        s.set_target_status("c", "n1", TargetStatus::Failed("err2".into()));

        let failed = s.failed_targets();
        assert_eq!(failed.len(), 2);
    }

    #[test]
    fn test_active_targets_grouped_by_node_sorted() {
        let mut s = make_state(vec!["z", "a", "m", "b"], vec!["n1", "n2"]);
        s.set_target_status("z", "n1", TargetStatus::Building);
        s.set_target_status("m", "n1", TargetStatus::Done(Duration::from_secs(1)));
        s.set_target_status("b", "n2", TargetStatus::Building);
        s.set_target_status("a", "n2", TargetStatus::Failed("err".into()));

        let active = s.active_targets();
        // n1 targets sorted: m, z
        // n2 targets sorted: a, b
        assert_eq!(active, vec!["m", "z", "a", "b"]);
    }

    #[test]
    fn test_active_targets_excludes_pending() {
        let mut s = make_state(vec!["a", "b", "c"], vec!["n1"]);
        s.set_target_status("a", "n1", TargetStatus::Building);
        // b and c remain pending
        let active = s.active_targets();
        assert_eq!(active, vec!["a"]);
    }

    #[test]
    fn test_is_complete_mixed() {
        let mut s = make_state(vec!["a", "b", "c"], vec!["n1"]);
        s.set_target_status("a", "n1", TargetStatus::Done(Duration::from_secs(1)));
        s.set_target_status("b", "n1", TargetStatus::Failed("err".into()));
        assert!(!s.is_complete()); // c is still pending

        s.set_target_status("c", "n1", TargetStatus::Done(Duration::from_secs(2)));
        assert!(s.is_complete());
    }

    #[test]
    fn cancelled_and_blocked_count_as_complete() {
        // #4: without this the fallback renderer loops forever after a cancel.
        let mut s = make_state(vec!["a", "b", "c"], vec!["n1"]);
        s.set_target_status("a", "n1", TargetStatus::Done(Duration::from_secs(1)));
        assert!(!s.is_complete());
        s.set_target_status("b", "", TargetStatus::Cancelled);
        assert!(!s.is_complete());
        s.set_target_status("c", "", TargetStatus::Blocked(vec!["a".into()]));
        assert!(s.is_complete());
        assert_eq!(s.count_skipped(), 2);
    }

    #[test]
    fn unassigned_targets_are_selectable() {
        // #12: blocked targets used to be counted but never rendered.
        let mut s = make_state(vec!["a", "b"], vec!["n1"]);
        s.set_target_status("a", "n1", TargetStatus::Building);
        s.set_target_status("b", "", TargetStatus::Blocked(vec!["a".into()]));

        assert_eq!(s.unassigned_targets().len(), 1);
        assert_eq!(s.active_targets(), vec!["a", "b"]);
    }

    #[test]
    fn a_target_on_an_unknown_node_is_still_rendered() {
        // Every non-pending target must appear somewhere, or the footer counts
        // rows the user cannot see.
        let mut s = make_state(vec!["a"], vec!["n1"]);
        s.set_target_status("a", "ghost-node", TargetStatus::Building);
        assert_eq!(s.unassigned_targets().len(), 1);
        assert_eq!(s.active_targets(), vec!["a"]);
    }

    #[test]
    fn pending_targets_are_not_unassigned() {
        let s = make_state(vec!["a"], vec!["n1"]);
        assert!(s.unassigned_targets().is_empty());
    }

    #[test]
    fn every_mutation_bumps_the_version() {
        let mut s = make_state(vec!["a"], vec!["n1"]);
        let v0 = s.version();
        s.set_target_status("a", "n1", TargetStatus::Building);
        let v1 = s.version();
        assert!(v1 > v0, "status change must be observable");
        s.update_progress("a", BuildProgress::default());
        assert!(s.version() > v1, "progress change must be observable");
    }

    #[test]
    fn terminal_states_are_classified() {
        assert!(TargetStatus::Done(Duration::from_secs(1)).is_terminal());
        assert!(TargetStatus::Failed("x".into()).is_terminal());
        assert!(TargetStatus::Cancelled.is_terminal());
        assert!(TargetStatus::Blocked(vec![]).is_terminal());
        assert!(!TargetStatus::Pending.is_terminal());
        assert!(!TargetStatus::Building.is_terminal());
    }

    #[test]
    fn test_node_reassignment() {
        let mut s = make_state(vec!["a"], vec!["n1", "n2"]);
        s.set_target_status("a", "n1", TargetStatus::Building);
        assert_eq!(s.targets["a"].node, "n1");

        // Reassign to different node (e.g., retry scenario)
        s.set_target_status("a", "n2", TargetStatus::Building);
        assert_eq!(s.targets["a"].node, "n2");
    }
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum TargetStatus {
    Pending,
    Building,
    Done(Duration),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub name: String,
    pub node: String,
    pub status: TargetStatus,
    pub started_at: Option<Instant>,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct DashboardState {
    pub targets: HashMap<String, TargetInfo>,
    pub target_order: Vec<String>,
    pub node_names: Vec<String>,
    pub total: usize,
    pub start_time: Instant,
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
                },
            );
        }
        Self {
            targets: target_map,
            target_order: targets,
            node_names,
            total,
            start_time: Instant::now(),
        }
    }

    pub fn set_target_status(&mut self, target: &str, node: &str, status: TargetStatus) {
        if let Some(info) = self.targets.get_mut(target) {
            info.node = node.to_string();
            if matches!(status, TargetStatus::Building) {
                info.started_at = Some(Instant::now());
            }
            info.status = status;
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
        self.count_done() + self.count_failed() == self.total
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
        result
    }
}

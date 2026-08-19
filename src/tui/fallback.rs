use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::state::{DashboardState, TargetStatus};
use crate::utils::lock_or_recover;

/// Tracks what has already been printed, so a poll loop can emit each
/// transition exactly once.
#[derive(Default)]
pub struct Reporter {
    announced: HashSet<String>,
    finished: HashSet<String>,
}

impl Reporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines to print for everything that changed since the last call.
    ///
    /// Separating this from the printing keeps the state lock off the stdout
    /// lock, and makes the "print once" rule testable.
    pub fn drain(&mut self, state: &DashboardState) -> Vec<String> {
        let mut lines = Vec::new();

        for name in &state.target_order {
            let Some(info) = state.targets.get(name) else {
                continue;
            };
            match &info.status {
                TargetStatus::Building => {
                    if self.announced.insert(name.clone()) {
                        lines.push(format!("[{}] building {}", info.node, name));
                    }
                }
                TargetStatus::Done(dur) => {
                    if self.finished.insert(name.clone()) {
                        lines.push(format!("✓ [{}] {} ({}s)", info.node, name, dur.as_secs()));
                    }
                }
                TargetStatus::Failed(err) => {
                    if self.finished.insert(name.clone()) {
                        lines.push(format!("✗ [{}] {} FAILED ({})", info.node, name, err));
                    }
                }
                TargetStatus::Cancelled => {
                    if self.finished.insert(name.clone()) {
                        lines.push(format!("- {} cancelled", name));
                    }
                }
                TargetStatus::Blocked(deps) => {
                    if self.finished.insert(name.clone()) {
                        lines.push(format!("- {} blocked by [{}]", name, deps.join(", ")));
                    }
                }
                TargetStatus::Pending => {}
            }
        }

        lines
    }
}

/// Line-by-line output for non-TTY or `--progress plain`.
///
/// Returns when every target has reached a terminal state, or when the run is
/// cancelled — never outliving the dispatcher it is paired with.
pub async fn run_fallback(state: Arc<Mutex<DashboardState>>, cancel: CancellationToken) {
    let mut reporter = Reporter::new();

    loop {
        let (lines, complete, summary) = {
            let s = lock_or_recover(&state);
            let lines = reporter.drain(&s);
            let complete = s.is_complete();
            let summary = complete.then(|| {
                let elapsed = s.elapsed();
                format!(
                    "---\n{} done, {} failed, {} skipped. Elapsed: {}m {:02}s",
                    s.count_done(),
                    s.count_failed(),
                    s.count_skipped(),
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60,
                )
            });
            (lines, complete, summary)
        };

        for line in lines {
            println!("{}", line);
        }

        if complete {
            if let Some(summary) = summary {
                println!("{}", summary);
            }
            break;
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                // The dispatcher settles every target before returning; give it
                // a moment, then stop regardless so we never outlive it.
                tokio::time::sleep(Duration::from_millis(200)).await;
                let s = lock_or_recover(&state);
                for line in reporter.drain(&s) {
                    println!("{}", line);
                }
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(targets: &[&str]) -> DashboardState {
        DashboardState::new(
            targets.iter().map(|s| s.to_string()).collect(),
            vec!["n1".to_string()],
        )
    }

    #[test]
    fn building_is_announced_exactly_once() {
        // The old loop re-printed the building line on every 500ms poll.
        let mut s = state(&["web"]);
        let mut r = Reporter::new();

        s.set_target_status("web", "n1", TargetStatus::Building);
        assert_eq!(r.drain(&s), vec!["[n1] building web".to_string()]);
        assert!(r.drain(&s).is_empty());
        assert!(r.drain(&s).is_empty());
    }

    #[test]
    fn completion_is_reported_after_the_building_line() {
        let mut s = state(&["web"]);
        let mut r = Reporter::new();

        s.set_target_status("web", "n1", TargetStatus::Building);
        assert_eq!(r.drain(&s).len(), 1);

        s.set_target_status("web", "n1", TargetStatus::Done(Duration::from_secs(3)));
        assert_eq!(r.drain(&s), vec!["✓ [n1] web (3s)".to_string()]);
        assert!(r.drain(&s).is_empty());
    }

    #[test]
    fn failures_and_skips_are_reported_once_each() {
        let mut s = state(&["a", "b", "c"]);
        let mut r = Reporter::new();

        s.set_target_status("a", "n1", TargetStatus::Failed("boom".into()));
        s.set_target_status("b", "", TargetStatus::Blocked(vec!["a".into()]));
        s.set_target_status("c", "", TargetStatus::Cancelled);

        let lines = r.drain(&s);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|l| l.contains("FAILED (boom)")));
        assert!(lines.iter().any(|l| l.contains("blocked by [a]")));
        assert!(lines.iter().any(|l| l.contains("c cancelled")));
        assert!(r.drain(&s).is_empty());
    }

    #[test]
    fn output_follows_declaration_order() {
        // Enough targets, in an order that is neither sorted nor alphabetical,
        // that HashMap iteration cannot pass by luck.
        let names = ["zeta", "alpha", "mike", "bravo", "yankee", "delta", "kilo"];
        let mut s = state(&names);
        let mut r = Reporter::new();
        for n in &names {
            s.set_target_status(n, "n1", TargetStatus::Building);
        }

        let lines = r.drain(&s);
        let reported: Vec<&str> = lines
            .iter()
            .map(|l| l.rsplit(' ').next().unwrap())
            .collect();
        assert_eq!(reported, names, "output must follow declaration order");
    }

    #[tokio::test]
    async fn returns_when_every_target_is_terminal() {
        let mut st = state(&["web"]);
        st.set_target_status("web", "n1", TargetStatus::Done(Duration::from_secs(1)));
        let shared = Arc::new(Mutex::new(st));

        tokio::time::timeout(
            Duration::from_secs(2),
            run_fallback(shared, CancellationToken::new()),
        )
        .await
        .expect("must return once complete");
    }

    #[tokio::test]
    async fn returns_on_cancellation_even_with_pending_targets() {
        // #4: with targets still Pending this used to loop forever, so
        // `tokio::join!(dispatch, run_fallback)` never returned.
        let shared = Arc::new(Mutex::new(state(&["a", "b"])));
        let cancel = CancellationToken::new();
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), run_fallback(shared, cancel))
            .await
            .expect("must return when cancelled");
    }
}

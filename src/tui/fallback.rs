use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::state::{DashboardState, TargetStatus};

/// Line-by-line output for non-TTY or --progress plain.
/// Polls state and prints changes.
pub async fn run_fallback(state: Arc<Mutex<DashboardState>>) {
    let mut reported: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        {
            let s = state.lock().unwrap();

            for (name, info) in &s.targets {
                match &info.status {
                    TargetStatus::Building if !reported.contains(name) => {
                        println!("[{}] building {}", info.node, name);
                        // Don't mark as reported yet — we want to report completion too
                    }
                    TargetStatus::Done(dur) if !reported.contains(name) => {
                        println!("✓ [{}] {} ({}s)", info.node, name, dur.as_secs());
                        reported.insert(name.clone());
                    }
                    TargetStatus::Failed(log) if !reported.contains(name) => {
                        println!("✗ [{}] {} FAILED ({})", info.node, name, log);
                        reported.insert(name.clone());
                    }
                    _ => {}
                }
            }

            if s.is_complete() {
                let elapsed = s.elapsed();
                println!(
                    "---\n{} done, {} failed. Elapsed: {}m {:02}s",
                    s.count_done(),
                    s.count_failed(),
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60,
                );
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

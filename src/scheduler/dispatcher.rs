use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::builder::node::ShardNode;
use crate::dag::DagQueue;
use crate::executor::bake::{execute_bake, BakeConfig, TargetCacheConfig};
use crate::tui::state::{DashboardState, TargetStatus};

/// Shared DAG queue wrapped for concurrent access.
/// Workers notify `wakeup` when a target completes so other workers
/// can check if new targets have been unblocked.
pub struct SharedDag {
    pub queue: std::sync::Mutex<DagQueue>,
    pub wakeup: Notify,
}

/// Per-node worker: claims one target at a time, builds it, repeats.
/// One-at-a-time ensures work-stealing naturally load-balances —
/// fast nodes finish sooner and grab the next target before slow nodes do.
async fn node_worker(
    node: ShardNode,
    dag: Arc<SharedDag>,
    state: Arc<std::sync::Mutex<DashboardState>>,
    config: Arc<BakeConfig>,
    cache_configs: Arc<HashMap<String, TargetCacheConfig>>,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Subscribe to wakeup BEFORE checking the queue to avoid race:
        // if a dependency completes between our claim() and .await,
        // we still get the notification.
        let notified = dag.wakeup.notified();

        // Try to claim a ready target compatible with this node's platforms
        let target = {
            let mut q = dag.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.claim_for_platforms(&node.platforms)
        };

        let target = match target {
            Some(t) => t,
            None => {
                let (done, stalled) = {
                    let q = dag.queue.lock().unwrap_or_else(|p| p.into_inner());
                    (q.is_done(), q.is_stalled())
                };

                if done || stalled {
                    break;
                }

                // Nothing ready yet — wait for a dependency to complete
                notified.await;
                continue;
            }
        };

        // Set log path and mark as building
        let log_path = std::env::temp_dir()
            .join("dbake-logs")
            .join(format!("{}.log", target));
        {
            let mut dashboard = state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(info) = dashboard.targets.get_mut(&target) {
                info.log_path = Some(log_path);
            }
            dashboard.set_target_status(&target, &node.name, TargetStatus::Building);
        }

        let target_cache = cache_configs.get(&target).cloned();
        let target_platforms = {
            let q = dag.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.target_platforms(&target).to_vec()
        };
        let start = std::time::Instant::now();
        let result = execute_bake(
            &node.shard_builder,
            &config,
            &target,
            target_cache.as_ref(),
            &target_platforms,
            Some(state.clone()),
            target.clone(),
        )
        .await;
        let elapsed = start.elapsed();

        {
            let mut dashboard = state.lock().unwrap_or_else(|p| p.into_inner());
            match &result {
                Ok(_) => {
                    dashboard.set_target_status(&target, &node.name, TargetStatus::Done(elapsed));
                }
                Err(e) => {
                    dashboard.set_target_status(
                        &target,
                        &node.name,
                        TargetStatus::Failed(e.to_string()),
                    );
                }
            }
        }

        {
            let mut queue = dag.queue.lock().unwrap_or_else(|p| p.into_inner());
            match result {
                Ok(_) => queue.complete(&target),
                Err(_) => {
                    queue.fail(&target);
                    if config.fail_fast {
                        cancel.cancel();
                    }
                }
            }
        }

        dag.wakeup.notify_waiters();
    }

    Ok(())
}

/// Dispatch all targets across all nodes, respecting DAG ordering.
pub async fn dispatch(
    nodes: Vec<ShardNode>,
    dag: DagQueue,
    config: BakeConfig,
    cache_configs: HashMap<String, TargetCacheConfig>,
    state: Arc<std::sync::Mutex<DashboardState>>,
    cancel: CancellationToken,
) -> Result<()> {
    // Check for dependency cycles before dispatching
    let cycles = dag.detect_cycles();
    if !cycles.is_empty() {
        eprintln!("\nWarning: dependency cycles detected:");
        for cycle in &cycles {
            eprintln!("  {} → {}", cycle.join(" → "), cycle[0]);
        }
        eprintln!("Targets in cycles will never be built.\n");
    }

    let shared_dag = Arc::new(SharedDag {
        queue: std::sync::Mutex::new(dag),
        wakeup: Notify::new(),
    });
    let config = Arc::new(config);
    let cache_configs = Arc::new(cache_configs);

    let mut handles = Vec::new();

    for node in nodes {
        let shared_dag = shared_dag.clone();
        let state = state.clone();
        let config = config.clone();
        let cache_configs = cache_configs.clone();
        let cancel = cancel.clone();

        let handle = tokio::spawn(node_worker(
            node,
            shared_dag,
            state,
            config,
            cache_configs,
            cancel,
        ));
        handles.push(handle);
    }

    for handle in handles {
        handle.await??;
    }

    // Report any targets blocked by failed dependencies
    let queue = shared_dag.queue.lock().unwrap_or_else(|p| p.into_inner());
    let blocked = queue.blocked_targets();
    if !blocked.is_empty() {
        eprintln!(
            "\nSkipped {} targets blocked by failed dependencies: {}",
            blocked.len(),
            blocked.join(", ")
        );
        drop(queue);
        let mut dashboard = state.lock().unwrap_or_else(|p| p.into_inner());
        for target in &blocked {
            dashboard.set_target_status(
                target,
                "",
                TargetStatus::Failed("blocked by failed dependency".into()),
            );
        }
    }

    Ok(())
}

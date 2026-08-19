use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::builder::node::ShardNode;
use crate::dag::DagQueue;
use crate::executor::bake::{execute_bake, BakeConfig, BuildError, TargetCacheConfig};
use crate::logs;
use crate::tui::state::{DashboardState, TargetStatus};
use crate::utils::lock_or_recover;

/// A single unit of work handed to the build step.
pub struct BuildRequest {
    pub shard_builder: String,
    pub target: String,
    pub platforms: Vec<String>,
    pub log_path: std::path::PathBuf,
}

type BuildFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;

/// How a target actually gets built.
///
/// Injected so the scheduling loop can be tested without docker: the loop is
/// where the deadlock and cancellation bugs lived, and it was previously
/// untestable because it called `execute_bake` directly.
pub type BuildStep = Arc<dyn Fn(BuildRequest, CancellationToken) -> BuildFuture + Send + Sync>;

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
    build: BuildStep,
    run_dir: Arc<std::path::PathBuf>,
    fail_fast: bool,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Register for wakeup BEFORE checking the queue: if a dependency
        // completes between our claim() and the .await, we must still get the
        // notification. `enable()` performs the registration up front rather
        // than on first poll.
        let mut notified = std::pin::pin!(dag.wakeup.notified());
        notified.as_mut().enable();

        // Try to claim a ready target compatible with this node's platforms
        let target = {
            let mut q = lock_or_recover(&dag.queue);
            q.claim_for_platforms(&node.platforms)
        };

        let target = match target {
            Some(t) => t,
            None => {
                let (done, stalled) = {
                    let q = lock_or_recover(&dag.queue);
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
        let log_path = logs::log_path(&run_dir, &target);
        {
            let mut dashboard = lock_or_recover(&state);
            dashboard.set_log_path(&target, log_path.clone());
            dashboard.set_target_status(&target, &node.name, TargetStatus::Building);
        }

        let target_platforms = {
            let q = lock_or_recover(&dag.queue);
            q.target_platforms(&target).to_vec()
        };
        let start = std::time::Instant::now();
        let result = build(
            BuildRequest {
                shard_builder: node.shard_builder.clone(),
                target: target.clone(),
                platforms: target_platforms,
                log_path,
            },
            cancel.clone(),
        )
        .await;
        let elapsed = start.elapsed();

        {
            let mut dashboard = lock_or_recover(&state);
            match &result {
                Ok(_) => {
                    dashboard.set_target_status(&target, &node.name, TargetStatus::Done(elapsed));
                }
                // Ask the error what happened rather than reading the shared
                // cancel token: with --fail-fast a genuine failure can land
                // after cancellation and must still be reported as a failure.
                Err(e) if was_cancelled(e) => {
                    dashboard.set_target_status(&target, &node.name, TargetStatus::Cancelled);
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
            let mut queue = lock_or_recover(&dag.queue);
            match result {
                Ok(_) => queue.complete(&target),
                // A cancelled target is terminal but not a failure, so its
                // dependents are not told a dependency failed.
                Err(ref e) if was_cancelled(e) => queue.cancel(&target),
                Err(_) => {
                    queue.fail(&target);
                    if fail_fast && !cancel.is_cancelled() {
                        eprintln!("--fail-fast: cancelling remaining builds");
                        cancel.cancel();
                    }
                }
            }
        }

        dag.wakeup.notify_waiters();
    }

    Ok(())
}

/// Whether this error is our own cancellation rather than a build failure.
fn was_cancelled(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|c| matches!(c.downcast_ref::<BuildError>(), Some(BuildError::Cancelled)))
}

/// Dispatch all targets across all nodes, respecting DAG ordering.
pub async fn dispatch(
    nodes: Vec<ShardNode>,
    dag: DagQueue,
    config: BakeConfig,
    cache_configs: HashMap<String, TargetCacheConfig>,
    run_dir: std::path::PathBuf,
    state: Arc<std::sync::Mutex<DashboardState>>,
    cancel: CancellationToken,
) -> Result<()> {
    let fail_fast = config.fail_fast;
    let config = Arc::new(config);
    let cache_configs = Arc::new(cache_configs);
    let build_state = state.clone();

    let build: BuildStep = Arc::new(move |req: BuildRequest, cancel: CancellationToken| {
        let config = config.clone();
        let cache_configs = cache_configs.clone();
        let state = build_state.clone();
        Box::pin(async move {
            let target_cache = cache_configs.get(&req.target).cloned();
            execute_bake(
                &req.shard_builder,
                &config,
                &req.target,
                target_cache.as_ref(),
                &req.platforms,
                Some(state),
                req.target.clone(),
                req.log_path,
                &cancel,
            )
            .await
            .map(|_| ())
        }) as BuildFuture
    });

    dispatch_with(nodes, dag, build, run_dir, fail_fast, state, cancel).await
}

/// Dispatch using an injected build step. See `dispatch` for the production wiring.
pub async fn dispatch_with(
    nodes: Vec<ShardNode>,
    dag: DagQueue,
    build: BuildStep,
    run_dir: std::path::PathBuf,
    fail_fast: bool,
    state: Arc<std::sync::Mutex<DashboardState>>,
    cancel: CancellationToken,
) -> Result<()> {
    // Tell the queue which nodes will actually run builds. Deriving this from
    // the worker list here means it cannot be forgotten by a caller — omitting
    // it silently restores the original "park forever" hang.
    let dag = dag.with_nodes(nodes.iter().map(|n| n.platforms.clone()).collect());

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
    let run_dir = Arc::new(run_dir);

    let mut handles = Vec::new();

    for node in nodes {
        let shared_dag = shared_dag.clone();
        let state = state.clone();
        let build = build.clone();
        let run_dir = run_dir.clone();
        let cancel = cancel.clone();

        let handle = tokio::spawn(node_worker(
            node, shared_dag, state, build, run_dir, fail_fast, cancel,
        ));
        handles.push(handle);
    }

    // Collect every worker result BEFORE propagating: the settle block below is
    // the only thing that drives targets to a terminal state, and a consumer
    // waiting for completion hangs forever if it is skipped.
    let mut worker_error = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => worker_error = worker_error.or(Some(e)),
            Err(e) => worker_error = worker_error.or(Some(e.into())),
        }
    }

    // Every target must reach a terminal state before we return: anything left
    // Pending would keep consumers waiting for completion forever.
    let queue = lock_or_recover(&shared_dag.queue);
    let unfinished = queue.unfinished_targets();
    let failed_deps: HashMap<String, Vec<String>> = unfinished
        .iter()
        .map(|t| (t.clone(), queue.failed_dependencies(t)))
        .collect();
    drop(queue);

    if !unfinished.is_empty() {
        let mut dashboard = lock_or_recover(&state);
        let mut blocked: Vec<String> = Vec::new();
        let mut cancelled = Vec::new();

        for target in &unfinished {
            if dashboard
                .targets
                .get(target)
                .is_some_and(|t| t.status.is_terminal())
            {
                continue;
            }
            let deps = failed_deps.get(target).cloned().unwrap_or_default();
            if deps.is_empty() {
                cancelled.push(target.clone());
                dashboard.set_target_status(target, "", TargetStatus::Cancelled);
            } else {
                blocked.push(format!("{} (needs {})", target, deps.join(", ")));
                dashboard.set_target_status(target, "", TargetStatus::Blocked(deps));
            }
        }
        drop(dashboard);

        if !blocked.is_empty() {
            eprintln!(
                "\nSkipped {} targets blocked by failed dependencies: {}",
                blocked.len(),
                blocked.join(", ")
            );
        }
        if !cancelled.is_empty() {
            eprintln!(
                "\nSkipped {} targets that never started: {}",
                cancelled.len(),
                cancelled.join(", ")
            );
        }
    }

    match worker_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn node(name: &str, platforms: &[&str]) -> ShardNode {
        ShardNode {
            name: name.to_string(),
            shard_builder: format!("shard-{name}"),
            platforms: platforms.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn state_for(targets: &[&str], nodes: &[&str]) -> Arc<std::sync::Mutex<DashboardState>> {
        Arc::new(std::sync::Mutex::new(DashboardState::new(
            names(targets),
            names(nodes),
        )))
    }

    /// A build step that records what it was asked to build.
    fn recording(
        log: Arc<std::sync::Mutex<Vec<String>>>,
        outcome: impl Fn(&str) -> Result<()> + Send + Sync + 'static,
    ) -> BuildStep {
        Arc::new(move |req: BuildRequest, _cancel| {
            log.lock().unwrap().push(req.target.clone());
            let result = outcome(&req.target);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                result
            }) as BuildFuture
        })
    }

    async fn run(
        nodes: Vec<ShardNode>,
        dag: DagQueue,
        build: BuildStep,
        fail_fast: bool,
        state: Arc<std::sync::Mutex<DashboardState>>,
        cancel: CancellationToken,
    ) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(10),
            dispatch_with(
                nodes,
                dag,
                build,
                std::env::temp_dir(),
                fail_fast,
                state,
                cancel,
            ),
        )
        .await
        .expect("dispatch must not hang")
    }

    #[tokio::test]
    async fn every_target_is_built_exactly_once_across_nodes() {
        let built = Arc::new(std::sync::Mutex::new(Vec::new()));
        let targets = names(&["a", "b", "c", "d", "e"]);
        let dag = DagQueue::new(targets.clone(), HashMap::new(), HashMap::new());
        let state = state_for(&["a", "b", "c", "d", "e"], &["n1", "n2", "n3"]);

        run(
            vec![
                node("n1", &["linux/amd64"]),
                node("n2", &["linux/amd64"]),
                node("n3", &["linux/amd64"]),
            ],
            dag,
            recording(built.clone(), |_| Ok(())),
            false,
            state.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let mut seen = built.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, targets, "each target built once, no duplicates");
        assert!(lock_or_recover(&state).is_complete());
        assert_eq!(lock_or_recover(&state).count_done(), 5);
    }

    #[tokio::test]
    async fn a_failure_blocks_its_dependents() {
        let built = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deps = HashMap::from([
            ("app".to_string(), names(&["base"])),
            ("tests".to_string(), names(&["app"])),
        ]);
        let dag = DagQueue::new(names(&["base", "app", "tests"]), deps, HashMap::new());
        let state = state_for(&["base", "app", "tests"], &["n1"]);

        run(
            vec![node("n1", &["linux/amd64"])],
            dag,
            recording(built.clone(), |t| {
                if t == "base" {
                    anyhow::bail!("boom")
                } else {
                    Ok(())
                }
            }),
            false,
            state.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(*built.lock().unwrap(), vec!["base".to_string()]);
        let s = lock_or_recover(&state);
        assert_eq!(s.count_failed(), 1);
        assert!(s.is_complete(), "dependents must reach a terminal state");
        assert!(matches!(
            s.targets["app"].status,
            TargetStatus::Blocked(ref d) if d == &names(&["base"])
        ));
    }

    #[tokio::test]
    async fn an_unclaimable_platform_target_does_not_hang() {
        // #2: the whole run used to park forever on this.
        let platforms = HashMap::from([("arm_only".to_string(), names(&["linux/arm64"]))]);
        let dag = DagQueue::new(names(&["a", "arm_only"]), HashMap::new(), platforms);
        let state = state_for(&["a", "arm_only"], &["n1", "n2"]);

        run(
            vec![node("n1", &["linux/amd64"]), node("n2", &["linux/amd64"])],
            dag,
            recording(Arc::new(std::sync::Mutex::new(Vec::new())), |_| Ok(())),
            false,
            state.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let s = lock_or_recover(&state);
        assert!(s.is_complete(), "unbuildable target must still settle");
        assert_eq!(s.count_done(), 1);
        assert_eq!(s.count_skipped(), 1);
    }

    #[tokio::test]
    async fn cancellation_drives_every_target_terminal() {
        // #4: leftover Pending targets made the plain-mode renderer hang.
        let dag = DagQueue::new(names(&["a", "b", "c", "d"]), HashMap::new(), HashMap::new());
        let state = state_for(&["a", "b", "c", "d"], &["n1"]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        run(
            vec![node("n1", &["linux/amd64"])],
            dag,
            recording(Arc::new(std::sync::Mutex::new(Vec::new())), |_| Ok(())),
            false,
            state.clone(),
            cancel,
        )
        .await
        .unwrap();

        let s = lock_or_recover(&state);
        assert!(
            s.is_complete(),
            "no target may be left Pending after a cancel"
        );
        assert_eq!(s.count_skipped(), 4);
    }

    #[tokio::test]
    async fn fail_fast_stops_scheduling_further_work() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let build: BuildStep = Arc::new(move |req: BuildRequest, _cancel| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let fail = req.target == "a";
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let _ = n;
                if fail {
                    anyhow::bail!("boom")
                }
                Ok(())
            }) as BuildFuture
        });

        let targets: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g", "h"];
        let dag = DagQueue::new(names(&targets), HashMap::new(), HashMap::new());
        let state = state_for(&targets, &["n1"]);

        run(
            vec![node("n1", &["linux/amd64"])],
            dag,
            build,
            true,
            state.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(
            count.load(Ordering::SeqCst) < targets.len(),
            "--fail-fast must stop scheduling, but every target was started"
        );
        assert!(lock_or_recover(&state).is_complete());
    }

    #[tokio::test]
    async fn dependencies_are_built_before_dependents() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deps = HashMap::from([
            ("app".to_string(), names(&["base"])),
            ("tests".to_string(), names(&["app"])),
        ]);
        let dag = DagQueue::new(names(&["base", "app", "tests"]), deps, HashMap::new());
        let state = state_for(&["base", "app", "tests"], &["n1", "n2"]);

        run(
            vec![node("n1", &["linux/amd64"]), node("n2", &["linux/amd64"])],
            dag,
            recording(order.clone(), |_| Ok(())),
            false,
            state.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(*order.lock().unwrap(), names(&["base", "app", "tests"]));
        assert!(lock_or_recover(&state).is_complete());
    }
}

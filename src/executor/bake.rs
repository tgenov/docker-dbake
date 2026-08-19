use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::bakeprint::CacheEntry;
use crate::tui::state::DashboardState;
use crate::utils::lock_or_recover;

use super::progress::ProgressParser;

/// Global configuration for bake execution, shared across all targets.
#[derive(Debug, Clone)]
pub struct BakeConfig {
    pub file: std::path::PathBuf,
    pub progress: String,
    pub cache_registry: Option<String>,
    pub no_cache: bool,
    pub load: bool,
    pub push: bool,
    pub fail_fast: bool,
}

/// Per-target cache configuration extracted from `docker buildx bake --print`.
#[derive(Debug, Clone, Default)]
pub struct TargetCacheConfig {
    pub cache_from: Vec<CacheEntry>,
    pub cache_to: Vec<CacheEntry>,
}

/// Execute `docker buildx bake` for a single target on a specific shard builder.
///
/// Streams stderr in real-time to parse BuildKit progress output, while teeing
/// both stdout and stderr to a log file. Progress updates are pushed to the
/// dashboard state via the `on_progress` callback.
#[allow(clippy::too_many_arguments)]
pub async fn execute_bake(
    builder: &str,
    config: &BakeConfig,
    target: &str,
    target_cache: Option<&TargetCacheConfig>,
    target_platforms: &[String],
    state: Option<Arc<Mutex<DashboardState>>>,
    target_name: String,
    log_path: std::path::PathBuf,
    cancel: &CancellationToken,
) -> Result<std::path::PathBuf> {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "bake"]);
    cmd.args(["--builder", builder]);
    cmd.args(["-f", &config.file.to_string_lossy()]);
    cmd.args(["--progress", &config.progress]);

    // Force the target's platform constraint so buildkit pulls the correct
    // architecture, even when running on a multi-arch or foreign-arch node.
    if !target_platforms.is_empty() {
        let platform_value = target_platforms.join(",");
        cmd.args(["--set", &format!("{}.platform={}", target, platform_value)]);
    }

    // A single `--set target.cache-from=` REPLACES whatever the bake file
    // declares; only repeated `--set` flags append to each other. So to add
    // registry cache *on top of* the file's own entries we must re-emit those
    // entries alongside it, correctly flattened to buildx's CSV form.
    if let Some(ref registry) = config.cache_registry {
        let registry_from = format!("type=registry,ref={}/buildcache/{}", registry, target);
        let registry_to = format!(
            "type=registry,ref={}/buildcache/{},mode=max",
            registry, target
        );

        let cache = target_cache.cloned().unwrap_or_default();

        for entry in cache
            .cache_from
            .iter()
            .map(CacheEntry::to_arg)
            .chain(std::iter::once(registry_from))
        {
            cmd.args(["--set", &format!("{}.cache-from={}", target, entry)]);
        }
        for entry in cache
            .cache_to
            .iter()
            .map(CacheEntry::to_arg)
            .chain(std::iter::once(registry_to))
        {
            cmd.args(["--set", &format!("{}.cache-to={}", target, entry)]);
        }
    }

    if config.no_cache {
        cmd.arg("--no-cache");
    }

    if config.load {
        cmd.arg("--load");
    }

    if config.push {
        cmd.arg("--push");
    }

    cmd.arg(target);

    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;
    let log_file = Arc::new(Mutex::new(log_file));

    // Pipe stdout and stderr for real-time progress parsing
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Backstop: an early return must never orphan a running build.
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to execute bake for target {}", target))?;

    let stderr = child.stderr.take().expect("stderr piped");
    let stdout = child.stdout.take().expect("stdout piped");

    // Spawn stderr reader — this is where BuildKit progress goes
    let log_clone = log_file.clone();
    let stderr_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut parser = ProgressParser::new();
        while let Ok(Some(line)) = lines.next_line().await {
            // Tee to log file
            if let Ok(mut f) = log_clone.lock() {
                use std::io::Write;
                writeln!(f, "{}", line).ok();
            }
            // Parse for progress updates
            if let Some(progress) = parser.parse_line(&line) {
                if let Some(ref state) = state {
                    let mut dashboard = lock_or_recover(state);
                    dashboard.update_progress(&target_name, progress);
                }
            }
        }
    });

    // Spawn stdout reader — just tee to log
    let log_clone2 = log_file.clone();
    let stdout_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut f) = log_clone2.lock() {
                use std::io::Write;
                writeln!(f, "{}", line).ok();
            }
        }
    });

    let outcome = wait_or_kill(&mut child, cancel)
        .await
        .with_context(|| format!("bake for target {} did not complete", target))?;

    // Wait for the output readers, but not forever: a grandchild that inherited
    // the pipe write end can keep them from ever seeing EOF, which would hang
    // cancellation even though the child itself is gone.
    let drain = tokio::time::timeout(Duration::from_secs(5), async {
        stderr_handle.await.ok();
        stdout_handle.await.ok();
    });
    drain.await.ok();

    match outcome {
        BuildOutcome::Killed => bail!(BuildError::Cancelled),
        BuildOutcome::Exited(status) if !status.success() => bail!(BuildError::Failed {
            target: target.to_string(),
            log: log_path.display().to_string(),
        }),
        BuildOutcome::Exited(_) => Ok(log_path),
    }
}

/// Why `execute_bake` returned an error, so the caller does not have to guess
/// from shared cancellation state.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("cancelled")]
    Cancelled,
    #[error("bake failed for target {target} (log: {log})")]
    Failed { target: String, log: String },
}

/// Why a build stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum BuildOutcome {
    /// The child exited on its own.
    Exited(std::process::ExitStatus),
    /// We terminated it because the run was cancelled.
    Killed,
}

/// How long buildx gets to tear down its BuildKit session after SIGTERM.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Wait for the child, terminating it if the run is cancelled.
///
/// Without this, cancelling only stops *scheduling*: builds already running
/// keep going and the process blocks until they finish.
///
/// The outcome is returned rather than folded into an error so the caller can
/// tell "we killed this" apart from "this build failed" — a genuine failure can
/// land inside the cancellation window and must not be reported as cancelled.
pub async fn wait_or_kill(
    child: &mut tokio::process::Child,
    cancel: &CancellationToken,
) -> Result<BuildOutcome> {
    tokio::select! {
        // Biased: a child that has already exited is reported on its own merits
        // even if cancellation fires in the same instant. Without this, select!
        // picks randomly and a build that succeeded can be recorded as killed.
        biased;
        status = child.wait() => Ok(BuildOutcome::Exited(status?)),
        _ = cancel.cancelled() => Ok(terminate(child).await),
    }
}

/// SIGTERM first, escalating to SIGKILL only if the child ignores it.
///
/// SIGKILL alone gives buildx no chance to close the BuildKit session, which
/// leaves the *remote* build running on the node.
async fn terminate(child: &mut tokio::process::Child) -> BuildOutcome {
    // If it already exited under its own steam, keep that status: a build can
    // fail microseconds before cancellation reaches it, and reporting that as
    // cancelled hides a real failure and its log.
    match child.try_wait() {
        Ok(Some(status)) => return BuildOutcome::Exited(status),
        Ok(None) => {}
        // waitpid failed, most likely ECHILD — the child is already reaped, so
        // signalling its pid would be signalling whatever now owns that number.
        Err(_) => return BuildOutcome::Killed,
    }

    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: the pid belongs to a child we own and have not reaped, so it
        // cannot have been recycled by another process.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if let Ok(Ok(status)) = tokio::time::timeout(TERM_GRACE, child.wait()).await {
            return classify_after_signal(status);
        }
    }

    child.start_kill().ok();
    let _ = child.wait().await;
    BuildOutcome::Killed
}

/// Interpret how a child exited *after* we signalled it.
///
/// Two failure modes have to be avoided at once, and they pull in opposite
/// directions:
///
/// * Reporting everything as `Killed` loses a build that was already failing on
///   its own merits when cancellation arrived — the user never sees its log.
/// * Reporting the raw status loses the fact that we stopped it: a process that
///   traps SIGTERM and exits 0 would be recorded as a successful build.
///
/// So: death *by* the signal, or a clean exit while shutting down, is a kill.
/// A non-zero exit the build chose for itself is a real failure. The worst case
/// is over-reporting a failure during cancellation, never claiming an
/// interrupted build succeeded.
fn classify_after_signal(status: std::process::ExitStatus) -> BuildOutcome {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal().is_some() {
            return BuildOutcome::Killed;
        }
    }
    match status.code() {
        Some(0) | None => BuildOutcome::Killed,
        Some(_) => BuildOutcome::Exited(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_or_kill_returns_status_when_not_cancelled() {
        let mut child = tokio::process::Command::new("true").spawn().unwrap();
        let cancel = CancellationToken::new();
        let outcome = wait_or_kill(&mut child, &cancel).await.unwrap();
        assert!(matches!(outcome, BuildOutcome::Exited(s) if s.success()));
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // SAFETY: signal 0 only performs error checking; it sends nothing.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_or_kill_terminates_the_process_for_real() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60")
            .spawn()
            .unwrap();
        let pid = child.id().expect("child should be running");
        assert!(process_exists(pid), "fixture did not start");

        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome =
            tokio::time::timeout(Duration::from_secs(8), wait_or_kill(&mut child, &cancel))
                .await
                .expect("must return promptly after cancellation")
                .unwrap();

        assert_eq!(
            outcome,
            BuildOutcome::Killed,
            "a cancelled build must be reported as killed, not as a failure"
        );
        // Ask the OS, rather than inferring death from the function returning.
        assert!(
            !process_exists(pid),
            "pid {pid} still exists — the build was abandoned, not terminated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_delivers_sigterm_before_sigkill() {
        // SIGKILL cannot be trapped, so a marker written from a TERM handler is
        // proof the child got a chance to shut down cleanly — which is what
        // lets buildx tear down the remote BuildKit session.
        let marker = std::env::temp_dir().join(format!("dbake-sigterm-{}", std::process::id()));
        std::fs::remove_file(&marker).ok();

        // `sleep 60 & wait` matters: a POSIX shell defers trap handlers until
        // the current FOREGROUND command finishes, so a bare `sleep 60` would
        // ignore SIGTERM until it ended — the fixture, not the code, would be
        // what the test measured.
        let script = format!(
            "trap 'touch {} ; exit 0' TERM; sleep 60 & wait",
            marker.display()
        );
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .spawn()
            .unwrap();

        // Give the shell a moment to install its trap.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome =
            tokio::time::timeout(Duration::from_secs(8), wait_or_kill(&mut child, &cancel))
                .await
                .expect("must not wait out the full grace period")
                .unwrap();

        assert_eq!(outcome, BuildOutcome::Killed);
        assert!(
            marker.exists(),
            "child never received SIGTERM — it was SIGKILLed outright"
        );
        std::fs::remove_file(&marker).ok();
    }

    #[tokio::test]
    async fn a_build_that_already_failed_keeps_its_status() {
        // --fail-fast cancels the run; a build that was already exiting non-zero
        // on its own merits must keep that status, or its log is never shown.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .unwrap();
        // Let it finish, but do not reap it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel = CancellationToken::new();
        cancel.cancel();

        match wait_or_kill(&mut child, &cancel).await.unwrap() {
            BuildOutcome::Exited(s) => assert_eq!(s.code(), Some(7)),
            BuildOutcome::Killed => {
                panic!("a self-inflicted failure was misreported as cancelled")
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_build_failing_inside_the_grace_window_keeps_its_failure() {
        // --fail-fast cancels the run while a second target is independently
        // failing. That failure, and its log, must not be relabelled cancelled.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("trap 'exit 3' TERM; sleep 60 & wait")
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel = CancellationToken::new();
        cancel.cancel();

        match wait_or_kill(&mut child, &cancel).await.unwrap() {
            BuildOutcome::Exited(s) => assert_eq!(s.code(), Some(3)),
            BuildOutcome::Killed => {
                panic!("a non-zero exit the build chose itself is a failure, not a cancel")
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_build_that_traps_sigterm_and_exits_zero_is_still_cancelled() {
        // buildx installs signal handlers; exiting 0 after SIGTERM must not be
        // recorded as a successful build.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("trap 'exit 0' TERM; sleep 60 & wait")
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel = CancellationToken::new();
        cancel.cancel();

        assert_eq!(
            wait_or_kill(&mut child, &cancel).await.unwrap(),
            BuildOutcome::Killed
        );
    }

    #[tokio::test]
    async fn a_build_that_finishes_first_is_not_reported_as_cancelled() {
        // `biased;` must make an already-exited child win the race, otherwise a
        // build that succeeded gets recorded as killed and marked failed.
        let mut child = tokio::process::Command::new("true").spawn().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = wait_or_kill(&mut child, &cancel).await.unwrap();
        assert!(
            matches!(outcome, BuildOutcome::Exited(s) if s.success()),
            "a finished build must be reported on its own merits, got {outcome:?}"
        );
    }
}

use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;

use tokio_util::sync::CancellationToken;

use super::state::{DashboardState, TargetStatus};
use crate::utils::lock_or_recover;

/// Current view mode for the dashboard.
enum View {
    /// Overview showing all nodes and targets.
    Overview { cursor: usize },
    /// Log viewer for a specific target, with scroll offset from bottom.
    Log { target: String, scroll: usize },
}

pub struct Dashboard {
    state: Arc<Mutex<DashboardState>>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
    cancel: CancellationToken,
    view: View,
    log_cache: LogTailCache,
    /// State version at the last redraw, so an idle dashboard stops re-rendering.
    last_version: u64,
    last_draw: std::time::Instant,
}

/// Caches the tail of a log file between frames.
///
/// Build logs reach tens of megabytes; re-reading the whole file ten times a
/// second to show forty lines makes the viewer unusable on exactly the long
/// builds it exists for.
#[derive(Default)]
struct LogTailCache {
    path: Option<std::path::PathBuf>,
    stamp: Option<(u64, Option<std::time::SystemTime>)>,
    lines: Vec<String>,
}

/// How much of the end of the file to keep buffered. Generous for any terminal.
const TAIL_BYTES: u64 = 256 * 1024;

impl LogTailCache {
    /// Lines to display, reading from disk only when the file has changed.
    fn view(&mut self, path: &std::path::Path, height: usize, scroll: usize) -> Vec<String> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return vec!["(log file not yet available)".to_string()],
        };

        // Length alone would miss a rewrite to the same size, so pair it with
        // the modification time.
        let stamp = (meta.len(), meta.modified().ok());
        if self.path.as_deref() != Some(path) || self.stamp != Some(stamp) {
            self.lines = read_tail(path, meta.len());
            self.path = Some(path.to_path_buf());
            self.stamp = Some(stamp);
        }

        if self.lines.is_empty() {
            return vec!["(log empty — build starting...)".to_string()];
        }

        // Clamp rather than going blank: `g` sets scroll to usize::MAX, and only
        // the last TAIL_BYTES are buffered, so scrolling past the top must land
        // on the oldest line we have.
        let total = self.lines.len();
        let scroll = scroll.min(total - 1);
        let end = total - scroll;
        let start = end.saturating_sub(height);
        self.lines[start..end].to_vec()
    }
}

/// Read the last `TAIL_BYTES` of a file as lines, dropping a leading partial line.
fn read_tail(path: &std::path::Path, len: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    // Start one byte BEFORE the window. That byte tells us whether the window
    // began on a line boundary: if it did, `lines()` yields a leading empty
    // element, and if it did not, it yields the tail of a cut line — either way
    // the first element is the one to drop, with no guessing.
    let mut from_start = len <= TAIL_BYTES;
    if !from_start {
        let probe = len - TAIL_BYTES - 1;
        // A probe of 0 means we are reading the whole file anyway, so its first
        // line is genuine and must not be trimmed as a partial one.
        if probe == 0 || file.seek(SeekFrom::Start(probe)).is_err() {
            from_start = true;
        }
    }

    let mut buf = Vec::with_capacity(TAIL_BYTES.min(len) as usize + 1);
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // Drop the leading partial (or empty) line — but never the only line we
    // have: a log whose tail holds no newline at all (a progress bar using \r)
    // would otherwise render as empty.
    if !from_start && lines.len() > 1 {
        lines.remove(0);
    }

    lines
}

impl Dashboard {
    pub fn new(
        state: Arc<Mutex<DashboardState>>,
        cancel: CancellationToken,
    ) -> anyhow::Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            state,
            terminal,
            cancel,
            view: View::Overview { cursor: 0 },
            log_cache: LogTailCache::default(),
            last_version: u64::MAX,
            last_draw: std::time::Instant::now(),
        })
    }

    /// Redraw only when the build state moved, the user pressed a key, or the
    /// on-screen clock is stale. Every frame re-reads the selected log file, so
    /// redrawing ten times a second regardless of change is real work.
    fn needs_redraw(&self) -> bool {
        let version = {
            let s = lock_or_recover(&self.state);
            s.version()
        };
        version != self.last_version || self.last_draw.elapsed() >= Duration::from_secs(1)
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        let mut dirty = true;

        loop {
            if dirty || self.needs_redraw() {
                // Read the version FIRST: a mutation landing between the draw
                // and this read would otherwise be stamped as already-drawn.
                let version = {
                    let s = lock_or_recover(&self.state);
                    s.version()
                };
                self.draw()?;
                self.last_version = version;
                self.last_draw = std::time::Instant::now();
            }
            dirty = false;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    dirty = true;
                    if key.kind == KeyEventKind::Press {
                        // Ctrl+C always cancels
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            self.cancel.cancel();
                            break;
                        }

                        match &mut self.view {
                            View::Overview { cursor } => match key.code {
                                KeyCode::Char('q') => {
                                    self.cancel.cancel();
                                    break;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    let count = {
                                        let s = lock_or_recover(&self.state);
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = (*cursor + 1) % count;
                                    }
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    let count = {
                                        let s = lock_or_recover(&self.state);
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = cursor.checked_sub(1).unwrap_or(count - 1);
                                    }
                                }
                                KeyCode::Enter => {
                                    let selected = {
                                        let s = lock_or_recover(&self.state);
                                        s.active_targets().get(*cursor).map(|s| s.to_string())
                                    };
                                    if let Some(name) = selected {
                                        self.view = View::Log {
                                            target: name,
                                            scroll: 0,
                                        };
                                    }
                                }
                                _ => {}
                            },
                            View::Log { scroll, .. } => match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    self.view = View::Overview { cursor: 0 };
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    *scroll = scroll.saturating_add(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    *scroll = scroll.saturating_sub(1);
                                }
                                KeyCode::Char('g') | KeyCode::Home => {
                                    *scroll = usize::MAX;
                                }
                                KeyCode::Char('G') | KeyCode::End => {
                                    *scroll = 0;
                                }
                                _ => {}
                            },
                        }
                    }
                }
            }

            if self.cancel.is_cancelled() {
                break;
            }

            let complete = {
                let s = lock_or_recover(&self.state);
                s.is_complete()
            };
            if complete {
                self.draw()?;
                std::thread::sleep(Duration::from_secs(1));
                break;
            }
        }

        Ok(())
    }

    fn draw(&mut self) -> anyhow::Result<()> {
        let state = self.state.clone();
        let view = &self.view;

        match view {
            View::Overview { cursor } => {
                let cursor = *cursor;
                self.terminal.draw(|frame| {
                    Self::draw_overview(frame, &state, cursor);
                })?;
            }
            View::Log { target, scroll } => {
                let target = target.clone();
                let scroll = *scroll;
                let log_path = {
                    let s = lock_or_recover(&state);
                    s.targets.get(&target).and_then(|i| i.log_path.clone())
                };
                // Header (2) + footer (2) + the log block's own borders (2).
                let height = self.terminal.size()?.height.saturating_sub(6) as usize;
                let lines = match &log_path {
                    Some(path) => self.log_cache.view(path, height, scroll),
                    None => vec!["(no log file)".to_string()],
                };
                self.terminal.draw(|frame| {
                    Self::draw_log_view(frame, &state, &target, scroll, lines);
                })?;
            }
        }

        Ok(())
    }

    fn draw_overview(frame: &mut Frame, state: &Arc<Mutex<DashboardState>>, cursor: usize) {
        let s = lock_or_recover(state);
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Header
                Constraint::Min(3),    // Active builds
                Constraint::Length(3), // Queue / idle
                Constraint::Length(3), // Footer
            ])
            .split(area);

        // Header
        let elapsed = s.elapsed();
        let header = format!(
            "docker dbake \u{2014} {} nodes, {} targets | {}m {:02}s",
            s.node_names.len(),
            s.total,
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60,
        );
        let header_widget = Paragraph::new(header).style(Style::default().fg(Color::Cyan).bold());
        frame.render_widget(header_widget, chunks[0]);

        // Build flat list of selectable targets for cursor highlighting
        let selectable: Vec<String> = s.active_targets().iter().map(|s| s.to_string()).collect();

        // Active builds — one line per non-pending target
        let mut lines = Vec::new();
        let bar_width = 10;

        // Node-assigned targets first, then anything that never reached a node
        // (cancelled or blocked) — otherwise those are counted in the footer
        // but never rendered.
        let mut rows: Vec<(String, &super::state::TargetInfo)> = Vec::new();
        for node_name in &s.node_names {
            for target in s.targets_for_node(node_name) {
                rows.push((short_node(node_name), target));
            }
        }
        for target in s.unassigned_targets() {
            rows.push(("skipped".to_string(), target));
        }

        {
            for (node_label, target) in &rows {
                let node_name = node_label;
                let is_selected = selectable.get(cursor).map(|s| s.as_str()) == Some(&target.name);
                let prefix = if is_selected { "\u{25b6} " } else { "  " };
                let line_bg = if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let line = match &target.status {
                    TargetStatus::Building => {
                        let (bar, pct_str, desc) = match &target.progress {
                            Some(p) if p.total_steps > 0 => {
                                let pct =
                                    (p.current_step as f64 / p.total_steps as f64 * 100.0) as u32;
                                let pct = pct.min(100);
                                let filled = (pct as usize * bar_width) / 100;
                                let empty = bar_width - filled;
                                let bar = format!(
                                    "{}{}",
                                    "\u{2588}".repeat(filled),
                                    "\u{2591}".repeat(empty)
                                );
                                let desc = match &p.stage {
                                    Some(stage) => {
                                        format!("[{}] {}", stage, p.step_description)
                                    }
                                    None => p.step_description.clone(),
                                };
                                (bar, format!("{:3}%", pct), truncate(&desc, 40))
                            }
                            _ => {
                                let elapsed = target
                                    .started_at
                                    .map(|s| s.elapsed().as_secs())
                                    .unwrap_or(0);
                                (
                                    "\u{2591}".repeat(bar_width),
                                    "  ?%".to_string(),
                                    format!("starting... {}s", elapsed),
                                )
                            }
                        };
                        Line::from(vec![
                            Span::styled(prefix, line_bg),
                            Span::styled(
                                format!("[{}]", node_name),
                                Style::default().fg(Color::Yellow).patch(line_bg),
                            ),
                            Span::styled(
                                format!(" {:<20} ", target.name),
                                Style::default().patch(line_bg),
                            ),
                            Span::styled(bar, Style::default().fg(Color::Green).patch(line_bg)),
                            Span::styled(format!(" {} ", pct_str), Style::default().patch(line_bg)),
                            Span::styled(desc, Style::default().fg(Color::DarkGray).patch(line_bg)),
                        ])
                    }
                    TargetStatus::Done(dur) => {
                        let bar = "\u{2588}".repeat(bar_width);
                        Line::from(vec![
                            Span::styled(prefix, line_bg),
                            Span::styled(
                                format!("[{}]", node_name),
                                Style::default().fg(Color::Yellow).patch(line_bg),
                            ),
                            Span::styled(
                                format!(" {:<20} ", target.name),
                                Style::default().patch(line_bg),
                            ),
                            Span::styled(bar, Style::default().fg(Color::Green).patch(line_bg)),
                            Span::styled(" 100% ", Style::default().patch(line_bg)),
                            Span::styled(
                                format!("done ({}s)", dur.as_secs()),
                                Style::default().fg(Color::Green).patch(line_bg),
                            ),
                        ])
                    }
                    TargetStatus::Failed(err) => {
                        let bar = "\u{2591}".repeat(bar_width);
                        let short_err = truncate(err, 40);
                        Line::from(vec![
                            Span::styled(prefix, line_bg),
                            Span::styled(
                                format!("[{}]", node_name),
                                Style::default().fg(Color::Yellow).patch(line_bg),
                            ),
                            Span::styled(
                                format!(" {:<20} ", target.name),
                                Style::default().patch(line_bg),
                            ),
                            Span::styled(bar, Style::default().fg(Color::Red).patch(line_bg)),
                            Span::styled(
                                " FAIL ",
                                Style::default().fg(Color::Red).bold().patch(line_bg),
                            ),
                            Span::styled(short_err, Style::default().fg(Color::Red).patch(line_bg)),
                        ])
                    }
                    TargetStatus::Cancelled | TargetStatus::Blocked(_) => {
                        let reason = match &target.status {
                            TargetStatus::Blocked(deps) => {
                                format!("blocked by [{}]", deps.join(", "))
                            }
                            _ => "cancelled".to_string(),
                        };
                        Line::from(vec![
                            Span::styled(prefix, line_bg),
                            Span::styled(
                                format!("[{}]", node_name),
                                Style::default().fg(Color::DarkGray).patch(line_bg),
                            ),
                            Span::styled(
                                format!(" {:<20} ", target.name),
                                Style::default().patch(line_bg),
                            ),
                            Span::styled(
                                "\u{2591}".repeat(bar_width),
                                Style::default().fg(Color::DarkGray).patch(line_bg),
                            ),
                            Span::styled(
                                " SKIP ",
                                Style::default().fg(Color::DarkGray).patch(line_bg),
                            ),
                            Span::styled(
                                truncate(&reason, 40),
                                Style::default().fg(Color::DarkGray).patch(line_bg),
                            ),
                        ])
                    }
                    TargetStatus::Pending => continue,
                };

                lines.push(line);
            }
        }

        if lines.is_empty() {
            lines.push(Line::styled(
                "  waiting for builds to start...",
                Style::default().fg(Color::DarkGray),
            ));
        }

        let builds_widget = Paragraph::new(lines);
        frame.render_widget(builds_widget, chunks[1]);

        // Queue + idle nodes
        let idle_nodes: Vec<&str> = s
            .node_names
            .iter()
            .filter(|n| {
                !s.targets
                    .values()
                    .any(|t| t.node == **n && matches!(t.status, TargetStatus::Building))
            })
            .map(|n| n.as_str())
            .collect();

        let pending = s.pending_targets();
        let mut queue_lines = Vec::new();

        if !pending.is_empty() {
            let shown: Vec<&str> = pending.iter().take(5).copied().collect();
            let extra = if pending.len() > 5 {
                format!(", +{} more", pending.len() - 5)
            } else {
                String::new()
            };
            queue_lines.push(Line::from(vec![
                Span::styled(
                    format!(" \u{25cb} {} pending", pending.len()),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!(" [{}{}]", shown.join(", "), extra)),
            ]));
        }

        if !idle_nodes.is_empty() {
            let idle_names: Vec<String> = idle_nodes.iter().map(|n| short_node(n)).collect();
            queue_lines.push(Line::from(vec![
                Span::styled(" [idle ] ", Style::default().fg(Color::DarkGray)),
                Span::raw(idle_names.join(", ")),
            ]));
        }

        if queue_lines.is_empty() {
            queue_lines.push(Line::raw(""));
        }

        let queue_widget =
            Paragraph::new(queue_lines).block(Block::default().borders(Borders::TOP));
        frame.render_widget(queue_widget, chunks[2]);

        // Footer
        let footer = format!(
            " \u{2713} {} done | \u{2699} {} building | \u{25cb} {} pending | \u{2717} {} failed | - {} skipped | j/k:select Enter:logs q:quit",
            s.count_done(),
            s.count_building(),
            s.count_pending(),
            s.count_failed(),
            s.count_skipped(),
        );
        let footer_widget = Paragraph::new(footer)
            .block(Block::default().borders(Borders::TOP))
            .style(Style::default().bold());
        frame.render_widget(footer_widget, chunks[3]);
    }

    fn draw_log_view(
        frame: &mut Frame,
        state: &Arc<Mutex<DashboardState>>,
        target: &str,
        scroll: usize,
        log_lines: Vec<String>,
    ) {
        let s = lock_or_recover(state);
        let area = frame.area();

        let (status_str, status_style) = match s.targets.get(target) {
            Some(info) => match &info.status {
                TargetStatus::Building => {
                    let elapsed = info.started_at.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                    (
                        format!("building ({}s)", elapsed),
                        Style::default().fg(Color::Blue),
                    )
                }
                TargetStatus::Done(dur) => (
                    format!("done ({}s)", dur.as_secs()),
                    Style::default().fg(Color::Green),
                ),
                TargetStatus::Failed(e) => {
                    (format!("FAILED: {}", e), Style::default().fg(Color::Red))
                }
                TargetStatus::Cancelled => (
                    "cancelled".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                TargetStatus::Blocked(deps) => (
                    format!("blocked by [{}]", deps.join(", ")),
                    Style::default().fg(Color::DarkGray),
                ),
                TargetStatus::Pending => ("pending".to_string(), Style::default()),
            },
            None => ("unknown".to_string(), Style::default()),
        };

        let node = s
            .targets
            .get(target)
            .map(|info| info.node.clone())
            .unwrap_or_default();

        drop(s); // Release lock before file I/O

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Header
                Constraint::Min(3),    // Log content
                Constraint::Length(2), // Footer
            ])
            .split(area);

        // Header
        let header = Line::from(vec![
            Span::styled(
                format!(" {} ", target),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Span::styled(format!("[{}] ", node), Style::default().fg(Color::Yellow)),
            Span::styled(status_str, status_style),
        ]);
        let header_widget = Paragraph::new(header);
        frame.render_widget(header_widget, chunks[0]);

        // Log content
        let text: Vec<Line> = log_lines.into_iter().map(Line::raw).collect();
        let log_widget =
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Build Log "));
        frame.render_widget(log_widget, chunks[1]);

        // Footer
        let scroll_hint = if scroll > 0 {
            format!(" +{} from bottom |", scroll)
        } else {
            " tail |".to_string()
        };
        let footer = format!(" Esc:back | j/k:scroll | g:top G:bottom |{}", scroll_hint);
        let footer_widget = Paragraph::new(footer).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer_widget, chunks[2]);
    }
}

/// Shorten a node name for the compact display.
/// e.g. "buildx_buildkit_zot-m3-pro0" -> "zot-m3-pro0"
fn short_node(name: &str) -> String {
    if let Some(pos) = name.rfind("buildkit_") {
        name[pos + 9..].to_string()
    } else {
        name.to_string()
    }
}

/// Truncate a string to `max_len` *characters*, adding "..." if truncated.
///
/// Counts characters rather than bytes: build step descriptions come from
/// Dockerfiles and routinely contain non-ASCII, and byte slicing panics when
/// the cut lands inside a multi-byte character.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    if max_len <= 3 {
        return s.chars().take(max_len).collect();
    }
    let mut out: String = s.chars().take(max_len - 3).collect();
    out.push_str("...");
    out
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("RUN make", 40), "RUN make");
    }

    #[test]
    fn truncate_shortens_long_ascii() {
        assert_eq!(truncate("abcdefghij", 8), "abcde...");
    }

    #[test]
    fn truncate_does_not_split_a_multibyte_char() {
        // Byte index 5 lands inside 'é'; slicing there panics.
        assert_eq!(truncate("abcdéfghijklmnop", 8), "abcdé...");
    }

    #[test]
    fn truncate_handles_an_all_unicode_description() {
        let s = "RUN echo ✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓✓";
        let out = truncate(s, 40);
        assert_eq!(out.chars().count(), 40);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_with_tiny_limit_is_not_a_panic() {
        assert_eq!(truncate("✓✓✓✓", 2).chars().count(), 2);
        assert_eq!(truncate("abcd", 0), "");
    }

    fn temp_log(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("dbake-tail-test-{}-{}", std::process::id(), name));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn tail_shows_the_last_lines_within_the_window() {
        let path = temp_log("small", b"a\nb\nc\nd\n");
        let mut cache = LogTailCache::default();
        assert_eq!(
            cache.view(&path, 2, 0),
            vec!["c".to_string(), "d".to_string()]
        );
        assert_eq!(cache.view(&path, 10, 0).len(), 4, "short file read whole");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_scrolls_back_from_the_end() {
        let path = temp_log("scroll", b"1\n2\n3\n4\n5\n");
        let mut cache = LogTailCache::default();
        assert_eq!(
            cache.view(&path, 2, 2),
            vec!["2".to_string(), "3".to_string()]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_newline_free_log_still_renders() {
        // A progress bar that only emits \r produces one enormous line. The
        // partial-line trim must not consume it and report the log as empty.
        let mut blob = vec![b'x'; (TAIL_BYTES as usize) + 4096];
        blob.extend_from_slice(b"END");
        let path = temp_log("noeol", &blob);

        let mut cache = LogTailCache::default();
        let view = cache.view(&path, 40, 0);
        assert_eq!(view.len(), 1, "expected the single long line, got {view:?}");
        assert!(view[0].ends_with("END"), "tail must reach the newest bytes");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_drops_only_the_partial_first_line_on_a_big_file() {
        let mut blob = vec![b'x'; (TAIL_BYTES as usize) + 10];
        blob.extend_from_slice(b"\nkept-1\nkept-2\n");
        let path = temp_log("big", &blob);

        let mut cache = LogTailCache::default();
        let view = cache.view(&path, 40, 0);
        assert_eq!(view, vec!["kept-1".to_string(), "kept-2".to_string()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_refreshes_when_the_file_grows() {
        let path = temp_log("growing", b"one\n");
        let mut cache = LogTailCache::default();
        assert_eq!(cache.view(&path, 10, 0), vec!["one".to_string()]);

        std::fs::write(&path, b"one\ntwo\n").unwrap();
        assert_eq!(
            cache.view(&path, 10, 0),
            vec!["one".to_string(), "two".to_string()],
            "an appending log must not render stale"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn scrolling_past_the_buffered_window_shows_the_oldest_lines() {
        // `g` sets scroll to usize::MAX and only the tail is buffered, so
        // scrolling past the top must land on the oldest line we have rather
        // than rendering a blank pane.
        let path = temp_log("clamp", b"1\n2\n3\n4\n5\n");
        let mut cache = LogTailCache::default();

        let top = cache.view(&path, 2, usize::MAX);
        assert!(!top.is_empty(), "scrolling to the top must not go blank");
        assert_eq!(top, vec!["1".to_string()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_log_is_reported_not_panicked() {
        let mut cache = LogTailCache::default();
        let view = cache.view(std::path::Path::new("/nonexistent/dbake.log"), 10, 0);
        assert_eq!(view.len(), 1);
        assert!(view[0].contains("not yet available"));
    }

    #[test]
    fn short_node_strips_the_buildkit_prefix() {
        assert_eq!(short_node("buildx_buildkit_zot-m3-pro0"), "zot-m3-pro0");
        assert_eq!(short_node("plain-node"), "plain-node");
    }
}

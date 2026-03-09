use std::io::{self, BufRead, Stdout};
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
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.draw()?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
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
                                        let s =
                                            self.state.lock().unwrap_or_else(|p| p.into_inner());
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = (*cursor + 1) % count;
                                    }
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    let count = {
                                        let s =
                                            self.state.lock().unwrap_or_else(|p| p.into_inner());
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = cursor.checked_sub(1).unwrap_or(count - 1);
                                    }
                                }
                                KeyCode::Enter => {
                                    let selected = {
                                        let s =
                                            self.state.lock().unwrap_or_else(|p| p.into_inner());
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
                let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
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

    /// Read the last N lines from a log file.
    fn read_log_tail(path: &std::path::Path, height: usize, scroll: usize) -> Vec<String> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return vec!["(log file not yet available)".to_string()],
        };
        let reader = io::BufReader::new(file);
        let all_lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

        if all_lines.is_empty() {
            return vec!["(log empty — build starting...)".to_string()];
        }

        let total = all_lines.len();
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(height);
        all_lines[start..end].to_vec()
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
                self.terminal.draw(|frame| {
                    Self::draw_log_view(frame, &state, &target, scroll);
                })?;
            }
        }

        Ok(())
    }

    fn draw_overview(frame: &mut Frame, state: &Arc<Mutex<DashboardState>>, cursor: usize) {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
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

        for node_name in &s.node_names {
            let targets = s.targets_for_node(node_name);
            for target in &targets {
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
                                (
                                    bar,
                                    format!("{:3}%", pct),
                                    truncate(&p.step_description, 40),
                                )
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
                                format!("[{}]", short_node(node_name)),
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
                                format!("[{}]", short_node(node_name)),
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
                                format!("[{}]", short_node(node_name)),
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
            " \u{2713} {} done | \u{2699} {} building | \u{25cb} {} pending | \u{2717} {} failed | j/k:select Enter:logs q:quit",
            s.count_done(),
            s.count_building(),
            s.count_pending(),
            s.count_failed(),
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
    ) {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
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
                TargetStatus::Pending => ("pending".to_string(), Style::default()),
            },
            None => ("unknown".to_string(), Style::default()),
        };

        let log_path = s.targets.get(target).and_then(|info| info.log_path.clone());

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
        let log_height = chunks[1].height as usize;
        let log_lines = match &log_path {
            Some(path) => Self::read_log_tail(path, log_height, scroll),
            None => vec!["(no log file)".to_string()],
        };

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

/// Truncate a string to max_len, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

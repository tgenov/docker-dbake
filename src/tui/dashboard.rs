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
                                        let s = self.state.lock().unwrap();
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = (*cursor + 1) % count;
                                    }
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    let count = {
                                        let s = self.state.lock().unwrap();
                                        s.active_targets().len()
                                    };
                                    if count > 0 {
                                        *cursor = cursor.checked_sub(1).unwrap_or(count - 1);
                                    }
                                }
                                KeyCode::Enter => {
                                    let selected = {
                                        let s = self.state.lock().unwrap();
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
                                    // Return cursor to the position of this target
                                    self.view = View::Overview { cursor: 0 };
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    *scroll = scroll.saturating_add(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    *scroll = scroll.saturating_sub(1);
                                }
                                KeyCode::Char('g') | KeyCode::Home => {
                                    // Scroll to very top (show beginning of file)
                                    *scroll = usize::MAX;
                                }
                                KeyCode::Char('G') | KeyCode::End => {
                                    // Scroll to bottom (latest output)
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
                let s = self.state.lock().unwrap();
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
        // scroll=0 means show the tail, scroll>0 means offset from bottom
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
        let s = state.lock().unwrap();
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Header
                Constraint::Min(4),    // Node sections
                Constraint::Length(3), // Queue
                Constraint::Length(3), // Footer
            ])
            .split(area);

        // Header
        let header = format!(
            "docker dbake — {} nodes, {} targets",
            s.node_names.len(),
            s.total,
        );
        let header_widget = Paragraph::new(header).style(Style::default().fg(Color::Cyan).bold());
        frame.render_widget(header_widget, chunks[0]);

        // Build flat list of selectable targets for cursor highlighting
        let selectable: Vec<String> = s.active_targets().iter().map(|s| s.to_string()).collect();

        // Node sections
        let node_constraints: Vec<Constraint> =
            s.node_names.iter().map(|_| Constraint::Min(3)).collect();
        let node_areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(node_constraints)
            .split(chunks[1]);

        for (i, node_name) in s.node_names.iter().enumerate() {
            if i >= node_areas.len() {
                break;
            }
            let targets = s.targets_for_node(node_name);
            let done_count = targets
                .iter()
                .filter(|t| matches!(t.status, TargetStatus::Done(_)))
                .count();
            let total_assigned = targets.len();

            let mut lines = Vec::new();

            let progress = if total_assigned > 0 {
                let pct = done_count as f64 / total_assigned as f64;
                let bar_width = 10;
                let filled = (pct * bar_width as f64) as usize;
                let empty = bar_width - filled;
                format!(
                    " {} {}/{}",
                    "\u{2588}".repeat(filled) + &"\u{2591}".repeat(empty),
                    done_count,
                    total_assigned
                )
            } else {
                let pending = s.count_pending();
                if pending > 0 {
                    format!(" idle \u{2014} {} targets waiting on dependencies", pending)
                } else {
                    " idle".to_string()
                }
            };

            let header_line = Line::from(vec![
                Span::styled(
                    format!(" {}", node_name),
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Span::raw(progress),
            ]);
            lines.push(header_line);

            for target in &targets {
                let (symbol, style, detail) = match &target.status {
                    TargetStatus::Done(dur) => (
                        "\u{2713}",
                        Style::default().fg(Color::Green),
                        format!("{}s", dur.as_secs()),
                    ),
                    TargetStatus::Building => {
                        let elapsed = target
                            .started_at
                            .map(|s| s.elapsed().as_secs())
                            .unwrap_or(0);
                        (
                            "\u{2699}",
                            Style::default().fg(Color::Blue),
                            format!("[building {}s...]", elapsed),
                        )
                    }
                    TargetStatus::Failed(_) => (
                        "\u{2717}",
                        Style::default().fg(Color::Red),
                        "FAILED".to_string(),
                    ),
                    TargetStatus::Pending => continue,
                };

                let is_selected = selectable.get(cursor).map(|s| s.as_str()) == Some(&target.name);
                let line_style = if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let prefix = if is_selected { " \u{25b6} " } else { "   " };

                lines.push(Line::from(vec![
                    Span::styled(prefix, line_style),
                    Span::styled(symbol, style.patch(line_style)),
                    Span::styled(format!(" {:24} {}", target.name, detail), line_style),
                ]));
            }

            let node_widget = Paragraph::new(lines);
            frame.render_widget(node_widget, node_areas[i]);
        }

        // Queue
        let pending = s.pending_targets();
        let queue_text = if pending.is_empty() {
            " Queue: empty".to_string()
        } else {
            let shown: Vec<&str> = pending.iter().take(5).copied().collect();
            let extra = if pending.len() > 5 {
                format!(", +{} more", pending.len() - 5)
            } else {
                String::new()
            };
            format!(
                " Queue: {} pending [{}{}]",
                pending.len(),
                shown.join(", "),
                extra
            )
        };
        let queue_widget = Paragraph::new(queue_text).block(Block::default().borders(Borders::TOP));
        frame.render_widget(queue_widget, chunks[2]);

        // Footer
        let elapsed = s.elapsed();
        let mins = elapsed.as_secs() / 60;
        let secs = elapsed.as_secs() % 60;
        let footer = format!(
            " \u{2713} {} done | \u{2699} {} building | \u{25cb} {} pending | \u{2717} {} failed | {}m {:02}s | j/k:select Enter:logs",
            s.count_done(),
            s.count_building(),
            s.count_pending(),
            s.count_failed(),
            mins,
            secs
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
        let s = state.lock().unwrap();
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

        let text: Vec<Line> = log_lines.into_iter().map(|l| Line::raw(l)).collect();
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

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

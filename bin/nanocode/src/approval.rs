use std::{
    io::Write,
    path::{Path, PathBuf},
};

use nanocode_tui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    overlay,
    ratatui::{
        layout::Rect,
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Paragraph, Widget, Wrap},
    },
    terminal::{Canvas, TerminalSession},
    theme::Theme,
};

use crate::{
    app::{App, TurnState, shlex_words},
    batch::Recorded,
    permissions::{Mode, Request},
};

const APPROVAL_FOOTER: &str =
    "y approve · A approve all waiting · n deny · v view diff · d diff tool · shift+tab mode";
const REVIEW_FOOTER: &str =
    "y keep all · r revert all · x revert file · ↑↓ file · v view diff · d diff tool · esc later";

pub struct Review {
    selected: usize,
    diffs: Vec<String>,
}

impl App {
    pub(crate) fn permission_mode(&self) -> Option<Mode> {
        self.host
            .config
            .permissions
            .as_ref()
            .map(|permissions| permissions.mode())
    }

    pub(crate) fn cycle_permissions(&mut self) {
        let Some(permissions) = self.host.config.permissions.clone() else {
            self.flash("permissions are not configured");
            return;
        };
        let mode = permissions.mode().next();
        permissions.set_mode(mode);
        let sandbox = if permissions.sandboxed() || matches!(mode, Mode::Auto | Mode::Manual) {
            ""
        } else {
            " · no sandbox here, so commands ask before running"
        };
        self.flash(format!("{} · {}{sandbox}", mode.label(), mode.describe()));
    }

    pub(crate) fn record_original(&mut self, recorded: Recorded) {
        if self
            .ledger
            .iter()
            .any(|existing| recorded.path.starts_with(&existing.path))
        {
            recorded.discard();
            return;
        }
        self.ledger.push(recorded);
        self.review_dismissed = false;
        if let Some(review) = &mut self.review {
            review.diffs = diffs(&self.ledger, &self.host.config.workspace);
        }
    }

    pub(crate) fn after_events(&mut self, terminal: &mut TerminalSession) -> bool {
        let mut changed = false;
        if self.review.is_none()
            && !self.review_dismissed
            && !self.ledger.is_empty()
            && matches!(self.turn, TurnState::Idle)
        {
            self.open_review(false);
            changed = true;
        }
        if std::mem::take(&mut self.pending_diff_tool) {
            self.open_diff_tool(terminal, true);
            changed = true;
        }
        changed
    }

    pub(crate) fn open_review(&mut self, requested: bool) {
        if self.ledger.is_empty() {
            if requested {
                self.flash("no changes to review");
            }
            return;
        }
        self.review = Some(Review {
            selected: 0,
            diffs: diffs(&self.ledger, &self.host.config.workspace),
        });
        self.review_dismissed = false;
        self.pending_diff_tool = self.host.config.auto_diff_tool;
    }

    pub(crate) fn approval_key(&mut self, key: KeyEvent, terminal: &mut TerminalSession) -> bool {
        self.approvals.retain(|request| !request.is_stale());
        if self.approvals.is_empty() {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('y' | 'a') | KeyCode::Enter => self.decide(true),
            KeyCode::Char('A') => {
                let count = self.approvals.len();
                for mut request in self.approvals.drain(..) {
                    request.answer(true);
                }
                self.flash(format!("approved {count}"));
            }
            KeyCode::Char('n') | KeyCode::Esc => self.decide(false),
            KeyCode::Char('c') if control => self.decide(false),
            KeyCode::Char('v') => {
                let diff = self
                    .approvals
                    .front()
                    .map(|request| request.diff.clone())
                    .unwrap_or_default();
                self.view_diff(terminal, diff);
            }
            KeyCode::Char('d') => self.open_diff_tool(terminal, false),
            KeyCode::Up | KeyCode::Char('k') => {
                self.approval_scroll = self.approval_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => self.approval_scroll += 1,
            _ => {}
        }
        true
    }

    pub(crate) fn review_key(&mut self, key: KeyEvent, terminal: &mut TerminalSession) -> bool {
        let Some(review) = &mut self.review else {
            return false;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let count = self.ledger.len();
                for recorded in self.ledger.drain(..) {
                    recorded.discard();
                }
                self.review = None;
                self.flash(format!("kept {count} changed paths"));
            }
            KeyCode::Char('r') => self.revert(None),
            KeyCode::Char('x') => {
                let selected = review.selected;
                self.revert(Some(selected));
            }
            KeyCode::Up | KeyCode::Char('k') => review.selected = review.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                review.selected = (review.selected + 1).min(self.ledger.len().saturating_sub(1));
            }
            KeyCode::Char('v') => {
                let diff = review.diffs.join("\n");
                self.view_diff(terminal, diff);
            }
            KeyCode::Char('d') => self.open_diff_tool(terminal, false),
            KeyCode::Esc => {
                self.review = None;
                self.review_dismissed = true;
                self.flash("review later with /review");
            }
            _ => {}
        }
        true
    }

    fn revert(&mut self, index: Option<usize>) {
        let mut targets = match index {
            Some(index) if index < self.ledger.len() => vec![self.ledger.remove(index)],
            Some(_) => return,
            None => std::mem::take(&mut self.ledger),
        };
        targets.sort_by_key(|recorded| recorded.path.components().count());
        let mut failures = Vec::new();
        for recorded in &targets {
            if let Err(error) = recorded.restore() {
                failures.push(format!("{}: {error}", recorded.path.display()));
            }
            recorded.discard();
        }
        let count = targets.len();
        if failures.is_empty() {
            self.flash(format!("reverted {count} changed paths"));
        } else {
            self.notice(
                nanocode_tui::transcript::NoticeLevel::Error,
                format!("could not revert: {}", failures.join("; ")),
            );
        }
        self.review = (!self.ledger.is_empty()).then(|| Review {
            selected: index.unwrap_or(0).min(self.ledger.len().saturating_sub(1)),
            diffs: diffs(&self.ledger, &self.host.config.workspace),
        });
    }

    fn decide(&mut self, approved: bool) {
        if let Some(mut request) = self.approvals.pop_front() {
            request.answer(approved);
            self.flash(if approved { "approved" } else { "denied" });
        }
        self.approval_scroll = 0;
        if !self.approvals.is_empty() {
            self.pending_diff_tool = self.host.config.auto_diff_tool;
        }
    }

    fn view_diff(&mut self, terminal: &mut TerminalSession, diff: String) {
        if diff.is_empty() {
            self.flash("there is no diff to show");
            return;
        }
        let Some(mut words) = self.viewer_words() else {
            self.flash("could not parse diff_viewer");
            return;
        };
        if words[0] == "delta" && !words.iter().any(|word| word.starts_with("--paging")) {
            words.push("--paging=always".to_owned());
        }
        drop(terminal.suspend());
        let result = std::process::Command::new(&words[0])
            .args(&words[1..])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    drop(stdin.write_all(diff.as_bytes()));
                }
                child.wait()
            });
        drop(terminal.resume());
        if let Err(error) = result {
            self.flash(format!("could not run {}: {error}", words[0]));
        }
    }

    fn viewer_words(&self) -> Option<Vec<String>> {
        let viewer = self
            .host
            .config
            .diff_viewer
            .clone()
            .or_else(|| on_path("delta").then(|| "delta".to_owned()))
            .or_else(|| std::env::var("PAGER").ok())
            .unwrap_or_else(|| "less -R".to_owned());
        shlex_words(&viewer).filter(|words| !words.is_empty())
    }

    fn preview(&mut self, diff: &str, width: u16) -> Vec<Line<'static>> {
        use std::hash::{Hash, Hasher};
        let limit = self.host.config.diff_preview_lines;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        diff.hash(&mut hasher);
        let key = (hasher.finish(), width);
        if let Some((cached, lines)) = &self.preview_cache
            && *cached == key
        {
            return lines.clone();
        }
        let rendered = self
            .viewer_words()
            .filter(|words| {
                Path::new(&words[0])
                    .file_name()
                    .is_some_and(|name| name == "delta")
            })
            .and_then(|words| delta(&words, diff, width))
            .unwrap_or_else(|| nanocode_tui::diff::render(diff, width, &self.theme));
        let total = rendered.len();
        let mut lines = rendered.into_iter().take(limit).collect::<Vec<_>>();
        if total > limit {
            lines.push(Line::styled(
                format!(
                    "… {} more lines · v to view all · d for the diff tool",
                    total - limit
                ),
                Style::default().fg(self.theme.muted()),
            ));
        }
        self.preview_cache = Some((key, lines.clone()));
        lines
    }

    fn diff_pairs(&self, every: bool) -> Vec<(PathBuf, PathBuf)> {
        if let Some(request) = self.approvals.front() {
            return request
                .files
                .iter()
                .map(|file| (file.old.clone(), file.new.clone()))
                .collect();
        }
        let Some(review) = &self.review else {
            return Vec::new();
        };
        self.ledger
            .iter()
            .enumerate()
            .filter(|(index, _)| every || *index == review.selected)
            .map(|(_, recorded)| (recorded.before(), recorded.after()))
            .collect()
    }

    fn open_diff_tool(&mut self, terminal: &mut TerminalSession, every: bool) {
        let pairs = self.diff_pairs(every);
        if pairs.is_empty() {
            return;
        }
        let tool = self
            .host
            .config
            .diff_tool
            .clone()
            .unwrap_or_else(|| "vimdiff".to_owned());
        let Some(words) = shlex_words(&tool).filter(|words| !words.is_empty()) else {
            self.flash(format!("could not parse diff_tool {tool:?}"));
            return;
        };
        drop(terminal.suspend());
        let mut failure = None;
        for (old, new) in pairs {
            let placeholders = words
                .iter()
                .any(|word| word.contains("{old}") || word.contains("{new}"));
            let mut arguments = words[1..]
                .iter()
                .map(|word| {
                    word.replace("{old}", &old.to_string_lossy())
                        .replace("{new}", &new.to_string_lossy())
                })
                .collect::<Vec<_>>();
            if !placeholders {
                arguments.push(old.to_string_lossy().into_owned());
                arguments.push(new.to_string_lossy().into_owned());
            }
            if let Err(error) = std::process::Command::new(&words[0])
                .args(arguments)
                .status()
            {
                failure = Some(format!("could not run {}: {error}", words[0]));
                break;
            }
        }
        drop(terminal.resume());
        if let Some(failure) = failure {
            self.flash(failure);
        }
    }
}

fn label(recorded: &Recorded, workspace: &std::path::Path) -> String {
    recorded
        .path
        .strip_prefix(workspace)
        .unwrap_or(&recorded.path)
        .display()
        .to_string()
}

fn diffs(ledger: &[Recorded], workspace: &std::path::Path) -> Vec<String> {
    ledger
        .iter()
        .map(|recorded| {
            crate::sandbox::unified_diff(
                &recorded.before(),
                &recorded.after(),
                &label(recorded, workspace),
            )
        })
        .collect()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

fn delta(words: &[String], diff: &str, width: u16) -> Option<Vec<Line<'static>>> {
    let mut child = std::process::Command::new(&words[0])
        .args(
            words[1..]
                .iter()
                .filter(|word| !word.starts_with("--paging")),
        )
        .args([
            "--paging=never",
            "--hunk-header-style=omit",
            "--file-decoration-style=none",
        ])
        .arg(format!("--width={width}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = diff.to_owned();
    let writer = std::thread::spawn(move || drop(stdin.write_all(input.as_bytes())));
    let output = child.wait_with_output().ok()?;
    drop(writer.join());
    output.status.success().then(|| {
        crate::ansi::lines(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .filter(|line| line.width() > 0)
            .collect()
    })
}

fn panel(
    area: Rect,
    frame: &mut Canvas<'_>,
    theme: &Theme,
    title: &str,
    footer: &str,
    lines: Vec<Line<'static>>,
    scroll: usize,
) -> usize {
    let width = area.width.saturating_sub(2);
    let rows = lines
        .iter()
        .map(|line| line.width().div_ceil(usize::from(width.max(1))).max(1))
        .sum::<usize>();
    let height = u16::try_from(rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_mul(3) / 4);
    let panel = overlay::bottom(area, area.width, height);
    let inner = overlay::frame(panel, frame.buffer_mut(), theme, title, Some(footer));
    let scroll = scroll.min(rows.saturating_sub(usize::from(inner.height)));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
        .render(inner, frame.buffer_mut());
    scroll
}

pub(crate) fn render(app: &mut App, area: Rect, frame: &mut Canvas<'_>) {
    app.approvals.retain(|request| !request.is_stale());
    let width = area.width.saturating_sub(2);
    let diff = match app.approvals.front() {
        Some(request) => Some(request.diff.clone()),
        None => app
            .review
            .as_ref()
            .and_then(|review| review.diffs.get(review.selected).cloned()),
    };
    let preview = diff
        .filter(|diff| !diff.is_empty())
        .map(|diff| app.preview(&diff, width));
    if let Some(request) = app.approvals.front() {
        let theme = &app.theme;
        let root = app.session_id();
        let mut lines = header(request, root.as_deref(), app.permission_mode(), theme);
        if let Some(preview) = preview {
            lines.push(Line::default());
            lines.extend(preview);
        }
        let pending = app.approvals.len();
        let title = if pending > 1 {
            format!("Permission · {} · 1 of {pending}", request.title)
        } else {
            format!("Permission · {}", request.title)
        };
        app.approval_scroll = panel(
            area,
            frame,
            theme,
            &title,
            APPROVAL_FOOTER,
            lines,
            app.approval_scroll,
        );
        return;
    }
    let Some(review) = &app.review else {
        return;
    };
    let theme = &app.theme;
    let workspace = &app.host.config.workspace;
    let mut lines = Vec::new();
    for (index, recorded) in app.ledger.iter().enumerate() {
        let selected = index == review.selected;
        let style = if selected {
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(theme.accent()),
            ),
            Span::styled(
                format!("{} ", recorded.status()),
                Style::default().fg(theme.thinking_high()),
            ),
            Span::styled(label(recorded, workspace), style),
        ]));
    }
    if let Some(preview) = preview {
        lines.push(Line::default());
        lines.extend(preview);
    }
    let title = format!("Review · {} changed paths", app.ledger.len());
    panel(area, frame, theme, &title, REVIEW_FOOTER, lines, 0);
}

fn header(
    request: &Request,
    root: Option<&str>,
    mode: Option<Mode>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(theme.muted());
    let mut lines = Vec::new();
    let mut origin = vec![Span::styled(
        format!("mode {}", mode.map_or("unset", Mode::label)),
        muted,
    )];
    if root.is_some_and(|root| root != request.session_id) {
        let short = request
            .session_id
            .get(request.session_id.len().saturating_sub(8)..)
            .unwrap_or(&request.session_id);
        origin.push(Span::styled(format!(" · subagent {short}"), muted));
    }
    lines.push(Line::from(origin));
    for (index, text) in request.detail.lines().enumerate() {
        let prefix = if index == 0 && request.command {
            "$ "
        } else {
            "  "
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.accent())),
            Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(theme.text())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    if !request.elsewhere.is_empty() {
        lines.push(Line::styled(
            format!(
                "also changes outside the repo (applied with the approval): {}",
                request.elsewhere.join(", ")
            ),
            Style::default().fg(theme.thinking_high()),
        ));
    }
    lines
}

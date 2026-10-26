use std::io::Write;

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
    app::{App, shlex_words},
    permissions::{Mode, Request},
};

const FOOTER: &str = "y approve · n deny · v view diff · d diff tool · ↑↓ scroll · shift+tab mode";

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

    pub(crate) fn approval_key(&mut self, key: KeyEvent, terminal: &mut TerminalSession) -> bool {
        self.approvals.retain(|request| !request.is_stale());
        if self.approvals.is_empty() {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('y' | 'a') | KeyCode::Enter => self.decide(true),
            KeyCode::Char('n') | KeyCode::Esc => self.decide(false),
            KeyCode::Char('c') if control => self.decide(false),
            KeyCode::Char('v') => self.view_diff(terminal),
            KeyCode::Char('d') => self.open_diff_tool(terminal),
            KeyCode::Up | KeyCode::Char('k') => {
                self.approval_scroll = self.approval_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => self.approval_scroll += 1,
            KeyCode::PageUp => self.approval_scroll = self.approval_scroll.saturating_sub(10),
            KeyCode::PageDown | KeyCode::Char(' ') => self.approval_scroll += 10,
            _ => {}
        }
        true
    }

    fn decide(&mut self, approved: bool) {
        if let Some(mut request) = self.approvals.pop_front() {
            request.answer(approved);
            self.flash(if approved { "approved" } else { "denied" });
        }
        self.approval_scroll = 0;
    }

    fn view_diff(&mut self, terminal: &mut TerminalSession) {
        let Some(request) = self.approvals.front() else {
            return;
        };
        if request.diff.is_empty() {
            self.flash("this request has no diff");
            return;
        }
        let viewer = self
            .host
            .config
            .diff_viewer
            .clone()
            .or_else(|| std::env::var("PAGER").ok())
            .unwrap_or_else(|| "less -R".to_owned());
        let Some(mut words) = shlex_words(&viewer).filter(|words| !words.is_empty()) else {
            self.flash(format!("could not parse diff_viewer {viewer:?}"));
            return;
        };
        if words[0] == "delta" && !words.iter().any(|word| word.starts_with("--paging")) {
            words.push("--paging=always".to_owned());
        }
        let diff = request.diff.clone();
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

    fn open_diff_tool(&mut self, terminal: &mut TerminalSession) {
        let Some(request) = self.approvals.front() else {
            return;
        };
        if request.files.is_empty() {
            self.flash("this request has no files to compare");
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
        let pairs = request
            .files
            .iter()
            .map(|file| (file.old.clone(), file.new.clone()))
            .collect::<Vec<_>>();
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

pub(crate) fn render(app: &mut App, area: Rect, frame: &mut Canvas<'_>) {
    app.approvals.retain(|request| !request.is_stale());
    let Some(request) = app.approvals.front() else {
        return;
    };
    let theme = &app.theme;
    let width = area.width.saturating_sub(2);
    let root = app.session_id();
    let mut lines = header(request, root.as_deref(), app.permission_mode(), theme);
    if !request.diff.is_empty() {
        lines.push(Line::default());
        lines.extend(nanocode_tui::diff::render(&request.diff, width, theme));
    }
    let rows = lines
        .iter()
        .map(|line| line.width().div_ceil(usize::from(width.max(1))).max(1))
        .sum::<usize>();
    let height = u16::try_from(rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_mul(3) / 4);
    let panel = overlay::bottom(area, area.width, height);
    let pending = app.approvals.len();
    let title = if pending > 1 {
        format!("Permission · {} · 1 of {pending}", request.title)
    } else {
        format!("Permission · {}", request.title)
    };
    let inner = overlay::frame(panel, frame.buffer_mut(), theme, &title, Some(FOOTER));
    let scroll = app
        .approval_scroll
        .min(lines.len().saturating_sub(usize::from(inner.height)));
    app.approval_scroll = scroll;
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
        .render(inner, frame.buffer_mut());
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

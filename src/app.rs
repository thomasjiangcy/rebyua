use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::cli::ReviewArgs;
use crate::clipboard;
use crate::export;
use crate::git::GitRepo;
use crate::model::{
    Annotation, AnnotationLineRange, AnnotationScope, ChangeKind, DiffKind, FilePatch, FileSummary,
    Focus, LineReference, PatchLine, SelectionRange,
};

const NOTIFICATION_TTL: Duration = Duration::from_secs(3);
const COMMENT_BOX_HEIGHT: u16 = 5;
const FOOTER_HEIGHT: u16 = 1;
const TWO_ROW_BREAKPOINT: u16 = 100;

pub fn run(args: ReviewArgs) -> Result<()> {
    let repo = GitRepo::discover(&args)?;
    let files = repo.load_files()?;

    let mut app = App::new(repo, files);
    let mut terminal = TerminalSession::new()?;

    while !app.should_quit {
        terminal
            .terminal
            .draw(|frame| app.render(frame))
            .context("failed to draw terminal frame")?;

        if event::poll(Duration::from_millis(100)).context("failed to poll terminal events")? {
            match event::read().context("failed to read terminal event")? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => app.on_key(key)?,
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        app.expire_notification();
    }

    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to create terminal backend")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug, Clone)]
struct HighlightedPatch {
    patch: FilePatch,
    flat_lines: Vec<PatchLine>,
    line_to_hunk: Vec<Option<usize>>,
    hunk_starts: Vec<usize>,
    highlights: Vec<Vec<StyledSegment>>,
}

#[derive(Debug, Clone)]
struct StyledSegment {
    text: String,
    style: Style,
}

#[derive(Debug, Clone)]
struct CommentDraft {
    target: CommentTarget,
    text: String,
}

#[derive(Debug, Clone, Copy)]
enum CommentTarget {
    File,
    Range(SelectionRange),
}

#[derive(Debug, Clone)]
struct Notification {
    message: String,
    created_at: Instant,
    kind: NotificationKind,
}

#[derive(Debug, Clone, Copy)]
enum NotificationKind {
    Success,
    Error,
}

struct App {
    repo: GitRepo,
    files: Vec<FileSummary>,
    filtered_file_indices: Vec<usize>,
    selected_file_view_idx: usize,
    focus: Focus,
    patch_cache: HashMap<String, HighlightedPatch>,
    diff_cursor: usize,
    diff_scroll: usize,
    selection: Option<SelectionRange>,
    comment_draft: Option<CommentDraft>,
    expanded_comment_line: Option<usize>,
    filter_input: Option<String>,
    filter_query: String,
    annotations: Vec<Annotation>,
    next_annotation_id: u64,
    notification: Option<Notification>,
    pending_quit_confirmation: bool,
    should_quit: bool,
    last_diff_inner_height: u16,
    syntax_set: SyntaxSet,
    syntax_theme: Theme,
}

impl App {
    fn new(repo: GitRepo, files: Vec<FileSummary>) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let syntax_theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| theme_set.themes.values().next().cloned())
            .unwrap_or_default();

        let mut app = Self {
            repo,
            files,
            filtered_file_indices: Vec::new(),
            selected_file_view_idx: 0,
            focus: Focus::Files,
            patch_cache: HashMap::new(),
            diff_cursor: 0,
            diff_scroll: 0,
            selection: None,
            comment_draft: None,
            expanded_comment_line: None,
            filter_input: None,
            filter_query: String::new(),
            annotations: Vec::new(),
            next_annotation_id: 1,
            notification: None,
            pending_quit_confirmation: false,
            should_quit: false,
            last_diff_inner_height: 0,
            syntax_set,
            syntax_theme,
        };
        app.refresh_filtered_files();
        if app.filtered_file_indices.is_empty() {
            app.focus = Focus::Files;
        } else {
            app.load_selected_patch();
        }
        app
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_filter_input(key) {
            return Ok(());
        }

        if self.handle_comment_input(key)? {
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.request_or_confirm_quit(),
            KeyCode::Char('h') => self.focus = Focus::Files,
            KeyCode::Char('l') | KeyCode::Enter if self.focus == Focus::Files => {
                if !self.filtered_file_indices.is_empty() {
                    self.focus = Focus::Diff;
                    self.load_selected_patch();
                }
            }
            KeyCode::Char('j') => self.move_down(),
            KeyCode::Char('k') => self.move_up(),
            KeyCode::Char('J') => self.move_hunk(1),
            KeyCode::Char('K') => self.move_hunk(-1),
            KeyCode::Char('g') => self.jump_first(),
            KeyCode::Char('G') => self.jump_last(),
            KeyCode::Char('v') => self.toggle_selection(),
            KeyCode::Char('c') => self.open_comment_draft(),
            KeyCode::Char('C') => self.open_file_comment_draft(),
            KeyCode::Enter if self.focus == Focus::Diff => self.inspect_current_comments(),
            KeyCode::Esc => self.clear_transient_state(),
            KeyCode::Char('/') => self.open_filter_input(),
            KeyCode::Char('E') => self.export_to_clipboard(),
            _ => {}
        }

        if !matches!(key.code, KeyCode::Char('q')) {
            self.pending_quit_confirmation = false;
        }

        Ok(())
    }

    fn handle_filter_input(&mut self, key: KeyEvent) -> bool {
        let Some(input) = self.filter_input.as_mut() else {
            return false;
        };

        match key.code {
            KeyCode::Esc => {
                self.filter_input = None;
            }
            KeyCode::Enter => {
                self.filter_query = input.clone();
                self.filter_input = None;
                self.refresh_filtered_files();
            }
            KeyCode::Backspace => {
                input.pop();
                self.filter_query = input.clone();
                self.refresh_filtered_files();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                self.filter_query.clear();
                self.refresh_filtered_files();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(ch);
                self.filter_query = input.clone();
                self.refresh_filtered_files();
            }
            _ => {}
        }

        true
    }

    fn handle_comment_input(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(draft) = self.comment_draft.as_mut() else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => self.comment_draft = None,
            KeyCode::Enter => self.save_comment()?,
            KeyCode::Backspace => {
                draft.text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.text.clear();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.text.push(ch);
            }
            KeyCode::Tab => draft.text.push_str("  "),
            _ => {}
        }

        Ok(true)
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Files => {
                if self.selected_file_view_idx + 1 < self.filtered_file_indices.len() {
                    self.selected_file_view_idx += 1;
                    self.reset_diff_view_for_selected_file();
                }
            }
            Focus::Diff => {
                let Some(patch) = self.current_patch() else {
                    return;
                };
                if self.diff_cursor + 1 < patch.flat_lines.len() {
                    self.diff_cursor += 1;
                    if let Some(selection) = self
                        .selection
                        .as_mut()
                        .filter(|selection| !selection.locked)
                    {
                        selection.cursor = self.diff_cursor;
                    }
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Files => {
                if self.selected_file_view_idx > 0 {
                    self.selected_file_view_idx -= 1;
                    self.reset_diff_view_for_selected_file();
                }
            }
            Focus::Diff => {
                if self.diff_cursor > 0 {
                    self.diff_cursor -= 1;
                    if let Some(selection) = self
                        .selection
                        .as_mut()
                        .filter(|selection| !selection.locked)
                    {
                        selection.cursor = self.diff_cursor;
                    }
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    fn move_hunk(&mut self, direction: isize) {
        let Some(patch) = self.current_patch() else {
            return;
        };
        if patch.hunk_starts.is_empty() {
            return;
        }

        let mut target = self.diff_cursor;
        if direction > 0 {
            for start in &patch.hunk_starts {
                if *start > self.diff_cursor {
                    target = *start;
                    break;
                }
            }
        } else {
            for start in patch.hunk_starts.iter().rev() {
                if *start < self.diff_cursor {
                    target = *start;
                    break;
                }
            }
        }

        self.diff_cursor = target;
        if let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| !selection.locked)
        {
            selection.cursor = target;
        }
        self.ensure_cursor_visible();
    }

    fn jump_first(&mut self) {
        match self.focus {
            Focus::Files => {
                self.selected_file_view_idx = 0;
                self.reset_diff_view_for_selected_file();
            }
            Focus::Diff => {
                self.diff_cursor = 0;
                if let Some(selection) = self.selection.as_mut() {
                    selection.cursor = 0;
                }
                self.ensure_cursor_visible();
            }
        }
    }

    fn jump_last(&mut self) {
        match self.focus {
            Focus::Files => {
                if !self.filtered_file_indices.is_empty() {
                    self.selected_file_view_idx = self.filtered_file_indices.len() - 1;
                    self.reset_diff_view_for_selected_file();
                }
            }
            Focus::Diff => {
                let Some(patch) = self.current_patch() else {
                    return;
                };
                if !patch.flat_lines.is_empty() {
                    self.diff_cursor = patch.flat_lines.len() - 1;
                    if let Some(selection) = self
                        .selection
                        .as_mut()
                        .filter(|selection| !selection.locked)
                    {
                        selection.cursor = self.diff_cursor;
                    }
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    fn toggle_selection(&mut self) {
        if self.focus != Focus::Diff || self.current_patch().is_none() {
            return;
        }

        match self.selection {
            None => {
                self.selection = Some(SelectionRange {
                    anchor: self.diff_cursor,
                    cursor: self.diff_cursor,
                    locked: false,
                });
            }
            Some(mut selection) => {
                selection.cursor = self.diff_cursor;
                selection.locked = !selection.locked;
                self.selection = Some(selection);
            }
        }
    }

    fn open_comment_draft(&mut self) {
        if self.focus != Focus::Diff || self.current_patch().is_none() {
            return;
        }

        let range = self.selection.unwrap_or(SelectionRange {
            anchor: self.diff_cursor,
            cursor: self.diff_cursor,
            locked: true,
        });
        self.expanded_comment_line = None;
        self.comment_draft = Some(CommentDraft {
            target: CommentTarget::Range(range),
            text: String::new(),
        });
        self.ensure_cursor_visible();
    }

    fn open_file_comment_draft(&mut self) {
        if self.selected_file_summary().is_none() {
            return;
        }

        self.focus = Focus::Diff;
        self.selection = None;
        self.expanded_comment_line = None;
        self.comment_draft = Some(CommentDraft {
            target: CommentTarget::File,
            text: String::new(),
        });
        self.diff_scroll = 0;
    }

    fn save_comment(&mut self) -> Result<()> {
        let Some(draft) = self.comment_draft.take() else {
            return Ok(());
        };
        let Some(summary) = self.selected_file_summary().cloned() else {
            return Ok(());
        };
        let Some(patch) = self.current_patch() else {
            return Ok(());
        };

        let body = draft.text.trim();
        if body.is_empty() {
            return Ok(());
        }

        let annotation = match draft.target {
            CommentTarget::File => Annotation::created_for_file(
                self.next_annotation_id,
                summary.path.clone(),
                body.to_string(),
            ),
            CommentTarget::Range(range) => {
                let (start, end) = range.normalized();
                let start_line = patch.flat_lines.get(start).context("invalid start line")?;
                let end_line = patch.flat_lines.get(end).context("invalid end line")?;
                let hunk_header = patch
                    .line_to_hunk
                    .get(start)
                    .and_then(|maybe_idx| maybe_idx.and_then(|idx| patch.patch.hunks.get(idx)))
                    .map(|hunk| hunk.header.clone());

                Annotation::created_for_lines(
                    self.next_annotation_id,
                    summary.path.clone(),
                    hunk_header,
                    AnnotationLineRange {
                        start_line_idx: start,
                        end_line_idx: end,
                        start_ref: LineReference {
                            old_lineno: start_line.old_lineno,
                            new_lineno: start_line.new_lineno,
                        },
                        end_ref: LineReference {
                            old_lineno: end_line.old_lineno,
                            new_lineno: end_line.new_lineno,
                        },
                    },
                    body.to_string(),
                )
            }
        };

        self.annotations.push(annotation);
        self.next_annotation_id += 1;
        self.selection = None;
        self.set_notification("Comment added".to_string(), NotificationKind::Success);
        Ok(())
    }

    fn inspect_current_comments(&mut self) {
        if self.focus == Focus::Files {
            self.focus = Focus::Diff;
            self.load_selected_patch();
            return;
        }

        let matching = self.comment_ids_on_current_line();
        if matching.is_empty() || self.expanded_comment_line == Some(self.diff_cursor) {
            self.expanded_comment_line = None;
        } else {
            self.expanded_comment_line = Some(self.diff_cursor);
            self.ensure_cursor_visible();
        }
    }

    fn clear_transient_state(&mut self) {
        if self.comment_draft.is_some() {
            self.comment_draft = None;
        } else if self.filter_input.is_some() {
            self.filter_input = None;
        } else if self.expanded_comment_line.is_some() {
            self.expanded_comment_line = None;
        } else {
            self.selection = None;
        }
    }

    fn open_filter_input(&mut self) {
        self.focus = Focus::Files;
        self.filter_input = Some(self.filter_query.clone());
    }

    fn export_to_clipboard(&mut self) {
        let markdown = export::markdown(&self.repo.base, &self.files, &self.annotations);
        match clipboard::copy_to_clipboard(&markdown) {
            Ok(()) => self.set_notification(
                "Review copied to clipboard".to_string(),
                NotificationKind::Success,
            ),
            Err(err) => self.set_notification(
                format!("Clipboard export failed: {err}"),
                NotificationKind::Error,
            ),
        }
    }

    fn request_or_confirm_quit(&mut self) {
        if self.annotations.is_empty() {
            self.should_quit = true;
            return;
        }

        if self.pending_quit_confirmation {
            self.should_quit = true;
            return;
        }

        self.pending_quit_confirmation = true;
        self.set_notification(
            "Unsaved in-memory comments will be lost. Press q again to quit.".to_string(),
            NotificationKind::Error,
        );
    }

    fn expire_notification(&mut self) {
        let Some(notification) = &self.notification else {
            return;
        };
        if notification.created_at.elapsed() >= NOTIFICATION_TTL {
            self.notification = None;
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(FOOTER_HEIGHT)])
            .split(frame.area());

        let main_chunks = if frame.area().width >= TWO_ROW_BREAKPOINT {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
                .split(root[0])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
                .split(root[0])
        };

        self.render_files(frame, main_chunks[0]);
        self.render_diff(frame, main_chunks[1]);
        self.render_footer(frame, root[1]);
    }

    fn render_files(&self, frame: &mut Frame, area: Rect) {
        let title = if let Some(filter_input) = &self.filter_input {
            format!(" Files /{} ", filter_input)
        } else if self.filter_query.is_empty() {
            " Files ".to_string()
        } else {
            format!(" Files ({}) ", self.filter_query)
        };

        let items: Vec<ListItem> = if self.filtered_file_indices.is_empty() {
            vec![ListItem::new(Line::from("No changed files"))]
        } else {
            self.filtered_file_indices
                .iter()
                .enumerate()
                .map(|(view_idx, file_idx)| {
                    let file = &self.files[*file_idx];
                    let counts = format!(
                        " +{} -{}",
                        file.added
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        file.deleted
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    );
                    let badge = change_badge(&file.change);
                    let comment_marker = if self.file_has_comments(&file.path) {
                        "● "
                    } else {
                        "  "
                    };
                    let path = if let Some(old_path) = &file.old_path {
                        format!("{old_path} -> {}", file.path)
                    } else {
                        file.path.clone()
                    };
                    let style =
                        if self.focus == Focus::Files && view_idx == self.selected_file_view_idx {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Rgb(208, 221, 255))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                    ListItem::new(Line::from(vec![
                        Span::styled(comment_marker, Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!("{badge} "),
                            Style::default().fg(change_color(&file.change)),
                        ),
                        Span::raw(path),
                        Span::styled(counts, Style::default().fg(Color::DarkGray)),
                    ]))
                    .style(style)
                })
                .collect()
        };

        let list = List::new(items).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(panel_border(self.focus == Focus::Files)),
        );
        frame.render_widget(list, area);
    }

    fn render_diff(&mut self, frame: &mut Frame, area: Rect) {
        let title = if let Some(summary) = self.selected_file_summary() {
            format!(" Diff {} ", summary.path)
        } else {
            " Diff ".to_string()
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(panel_border(self.focus == Focus::Diff));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.last_diff_inner_height = inner.height;

        let Some(patch) = self.current_patch() else {
            let empty = Paragraph::new("Select a file to review.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false });
            frame.render_widget(empty, inner);
            return;
        };

        let has_file_comment_ui = !self.file_comments_for_selected_file().is_empty()
            || matches!(
                self.comment_draft.as_ref().map(|draft| draft.target),
                Some(CommentTarget::File)
            );

        if patch.flat_lines.is_empty() && !has_file_comment_ui {
            let message = if patch.patch.metadata.is_empty() {
                "No textual patch available.".to_string()
            } else {
                patch.patch.metadata.join("\n")
            };
            frame.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
            return;
        }

        let items = self.diff_items(patch);
        let mut y = 0u16;
        let mut visual_offset = 0usize;

        for item in items {
            if visual_offset + item.height as usize <= self.diff_scroll {
                visual_offset += item.height as usize;
                continue;
            }
            if y >= inner.height {
                break;
            }

            let skip_rows = self.diff_scroll.saturating_sub(visual_offset);
            let available_height = inner.height.saturating_sub(y);
            let render_height = item
                .height
                .saturating_sub(skip_rows as u16)
                .min(available_height);
            if render_height == 0 {
                visual_offset += item.height as usize;
                continue;
            }

            let row_area = Rect {
                x: inner.x,
                y: inner.y + y,
                width: inner.width,
                height: render_height,
            };

            match item.kind {
                DiffItemKind::FileComments => {
                    self.render_file_comments(frame, row_area);
                }
                DiffItemKind::Line(line_idx) => {
                    self.render_diff_line(frame, row_area, patch, line_idx);
                }
                DiffItemKind::Editor => {
                    self.render_comment_editor(frame, row_area);
                }
                DiffItemKind::ExpandedComments { line_idx } => {
                    self.render_expanded_comments(frame, row_area, line_idx);
                }
            }

            y += render_height;
            visual_offset += item.height as usize;
        }
    }

    fn render_diff_line(
        &self,
        frame: &mut Frame,
        area: Rect,
        patch: &HighlightedPatch,
        line_idx: usize,
    ) {
        let line = &patch.flat_lines[line_idx];
        let in_selection = self
            .selected_range()
            .map(|(start, end)| line_idx >= start && line_idx <= end)
            .unwrap_or(false);
        let has_comments = self.line_has_comments(line_idx);
        let selected = self.focus == Focus::Diff && line_idx == self.diff_cursor;

        let base_style = diff_base_style(line.kind);
        let line_style = if selected {
            base_style.bg(Color::Rgb(50, 61, 82))
        } else if in_selection {
            base_style.bg(Color::Rgb(32, 42, 58))
        } else {
            base_style
        };

        let gutter = if has_comments { "●" } else { " " };
        let sign = match line.kind {
            DiffKind::Add => "+",
            DiffKind::Delete => "-",
            DiffKind::Context => " ",
        };

        let old = line
            .old_lineno
            .map(|value| format!("{value:>4}"))
            .unwrap_or_else(|| "    ".to_string());
        let new = line
            .new_lineno
            .map(|value| format!("{value:>4}"))
            .unwrap_or_else(|| "    ".to_string());

        let mut spans = vec![
            Span::styled(format!("{gutter} "), Style::default().fg(Color::Yellow)),
            Span::styled(old, Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::styled(new, Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::styled(sign, Style::default().fg(diff_sign_color(line.kind))),
            Span::raw(" "),
        ];

        let highlighted = patch.highlights.get(line_idx).cloned().unwrap_or_default();
        if highlighted.is_empty() {
            spans.push(Span::raw(line.text.clone()));
        } else {
            for segment in highlighted {
                spans.push(Span::styled(segment.text, segment.style));
            }
        }

        frame.render_widget(Paragraph::new(Line::from(spans)).style(line_style), area);
    }

    fn render_comment_editor(&self, frame: &mut Frame, area: Rect) {
        let is_file_comment = matches!(
            self.comment_draft.as_ref().map(|draft| draft.target),
            Some(CommentTarget::File)
        );
        let title = if is_file_comment {
            " File Comment  Enter save  Esc cancel "
        } else {
            " Comment  Enter save  Esc cancel "
        };
        let border_color = if is_file_comment {
            Color::Rgb(113, 205, 205)
        } else {
            Color::Rgb(216, 180, 84)
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let draft_text = self
            .comment_draft
            .as_ref()
            .map(|draft| draft.text.as_str())
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(draft_text)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::White)),
            inner,
        );
    }

    fn render_expanded_comments(&self, frame: &mut Frame, area: Rect, line_idx: usize) {
        let comments = self.comments_on_line(line_idx);
        let block = Block::default()
            .title(" Comments ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(110, 130, 170)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines = Vec::new();
        for comment in comments {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Color::Yellow)),
                Span::raw(comment.body.clone()),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::from("No comments."));
        }

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::White)),
            inner,
        );
    }

    fn render_file_comments(&self, frame: &mut Frame, area: Rect) {
        let comments = self.file_comments_for_selected_file();
        let block = Block::default()
            .title(" File Comments ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(110, 130, 170)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = comments
            .into_iter()
            .map(|comment| {
                Line::from(vec![
                    Span::styled("• ", Style::default().fg(Color::Yellow)),
                    Span::raw(comment.body.clone()),
                ])
            })
            .collect();

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::White)),
            inner,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let status = if let Some(notification) = &self.notification {
            let color = match notification.kind {
                NotificationKind::Success => Color::Rgb(149, 198, 136),
                NotificationKind::Error => Color::Rgb(224, 110, 110),
            };
            Line::from(vec![Span::styled(
                notification.message.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )])
        } else if let Some(filter) = &self.filter_input {
            Line::from(vec![Span::styled(
                format!("Filtering: {filter}"),
                Style::default().fg(Color::Rgb(160, 196, 255)),
            )])
        } else if self.comment_draft.is_some() {
            Line::from("Comment mode: type to write, Enter to save, Esc to cancel")
        } else {
            Line::from(
                "h/l focus  j/k move  v select  c line  C file  Enter inspect  / filter  E copy",
            )
        };

        frame.render_widget(Paragraph::new(status), area);
    }

    fn refresh_filtered_files(&mut self) {
        self.filtered_file_indices = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                self.filter_query.is_empty() || file.path.contains(&self.filter_query)
            })
            .map(|(idx, _)| idx)
            .collect();

        if self.filtered_file_indices.is_empty() {
            self.selected_file_view_idx = 0;
            self.diff_cursor = 0;
            self.diff_scroll = 0;
            self.selection = None;
            self.comment_draft = None;
            self.expanded_comment_line = None;
        } else if self.selected_file_view_idx >= self.filtered_file_indices.len() {
            self.selected_file_view_idx = self.filtered_file_indices.len() - 1;
            self.reset_diff_view_for_selected_file();
        } else {
            self.load_selected_patch();
        }
    }

    fn reset_diff_view_for_selected_file(&mut self) {
        self.diff_cursor = 0;
        self.diff_scroll = 0;
        self.selection = None;
        self.comment_draft = None;
        self.expanded_comment_line = None;
        self.load_selected_patch();
    }

    fn load_selected_patch(&mut self) {
        let Some(summary) = self.selected_file_summary().cloned() else {
            return;
        };
        if self.patch_cache.contains_key(&summary.path) {
            return;
        }
        match self.repo.load_patch(&summary) {
            Ok(patch) => {
                let highlighted = self.highlight_patch(patch);
                self.patch_cache.insert(summary.path.clone(), highlighted);
            }
            Err(err) => {
                let fallback = FilePatch {
                    summary: summary.clone(),
                    hunks: Vec::new(),
                    metadata: vec![format!("Failed to load patch: {err}")],
                };
                let highlighted = self.highlight_patch(fallback);
                self.patch_cache.insert(summary.path.clone(), highlighted);
                self.set_notification(
                    format!("Failed to load {}: {err}", summary.path),
                    NotificationKind::Error,
                );
            }
        }
    }

    fn highlight_patch(&self, patch: FilePatch) -> HighlightedPatch {
        let syntax = self
            .syntax_set
            .find_syntax_for_file(&patch.summary.path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, &self.syntax_theme);
        let mut flat_lines = Vec::new();
        let mut line_to_hunk = Vec::new();
        let mut hunk_starts = Vec::new();
        let mut highlights = Vec::new();

        for (hunk_idx, hunk) in patch.hunks.iter().enumerate() {
            hunk_starts.push(flat_lines.len());
            for line in &hunk.lines {
                let segments =
                    highlight_line_segments(&self.syntax_set, &mut highlighter, &line.text);
                flat_lines.push(line.clone());
                line_to_hunk.push(Some(hunk_idx));
                highlights.push(segments);
            }
        }

        HighlightedPatch {
            patch,
            flat_lines,
            line_to_hunk,
            hunk_starts,
            highlights,
        }
    }

    fn diff_items(&self, patch: &HighlightedPatch) -> Vec<DiffItem> {
        let mut items = Vec::new();
        let file_comments_count = self.file_comments_for_selected_file().len();
        let show_file_comments = file_comments_count > 0;
        let file_editor = matches!(
            self.comment_draft.as_ref().map(|draft| draft.target),
            Some(CommentTarget::File)
        );
        let editor_anchor = self
            .comment_draft
            .as_ref()
            .and_then(|draft| match draft.target {
                CommentTarget::File => None,
                CommentTarget::Range(range) => Some(range.normalized().1),
            });
        let expanded_anchor = self.expanded_comment_line.and_then(|line_idx| {
            let comments = self.comments_on_line(line_idx);
            comments
                .iter()
                .filter_map(|comment| comment.line_range().map(|(_, end)| end))
                .max()
                .or(Some(line_idx))
        });

        if show_file_comments {
            items.push(DiffItem {
                kind: DiffItemKind::FileComments,
                height: file_comments_height(file_comments_count),
            });
        }
        if file_editor {
            items.push(DiffItem {
                kind: DiffItemKind::Editor,
                height: COMMENT_BOX_HEIGHT,
            });
        }

        for line_idx in 0..patch.flat_lines.len() {
            items.push(DiffItem {
                kind: DiffItemKind::Line(line_idx),
                height: 1,
            });
            if Some(line_idx) == editor_anchor {
                items.push(DiffItem {
                    kind: DiffItemKind::Editor,
                    height: COMMENT_BOX_HEIGHT,
                });
            }
            if Some(line_idx) == expanded_anchor {
                items.push(DiffItem {
                    kind: DiffItemKind::ExpandedComments { line_idx },
                    height: COMMENT_BOX_HEIGHT,
                });
            }
        }

        items
    }

    fn ensure_cursor_visible(&mut self) {
        let height = self.last_diff_inner_height.saturating_sub(1) as usize;
        if height == 0 {
            return;
        }

        let Some(patch) = self.current_patch() else {
            return;
        };
        let items = self.diff_items(patch);
        let mut line_visual_row = 0usize;
        let mut editor_end = None;

        for item in &items {
            match item.kind {
                DiffItemKind::Line(line_idx) if line_idx == self.diff_cursor => break,
                DiffItemKind::FileComments => {
                    line_visual_row += item.height as usize;
                }
                DiffItemKind::Line(_)
                | DiffItemKind::Editor
                | DiffItemKind::ExpandedComments { .. } => {
                    line_visual_row += item.height as usize;
                }
            }
        }

        if let Some(draft) = &self.comment_draft {
            match draft.target {
                CommentTarget::File => {
                    editor_end = Some(
                        items
                            .iter()
                            .take_while(|item| !matches!(item.kind, DiffItemKind::Line(_)))
                            .map(|item| item.height as usize)
                            .sum(),
                    );
                }
                CommentTarget::Range(range) => {
                    let anchor = range.normalized().1;
                    let mut offset = 0usize;
                    for item in &items {
                        match item.kind {
                            DiffItemKind::Line(line_idx) if line_idx == anchor => {
                                offset += 1;
                            }
                            DiffItemKind::Editor => {
                                editor_end = Some(offset + item.height as usize);
                                break;
                            }
                            _ => offset += item.height as usize,
                        }
                    }
                }
            }
        } else if self.expanded_comment_line.is_some() {
            let mut offset = 0usize;
            for item in &items {
                if matches!(item.kind, DiffItemKind::ExpandedComments { .. }) {
                    editor_end = Some(offset + item.height as usize);
                    break;
                }
                offset += item.height as usize;
            }
        }

        if line_visual_row < self.diff_scroll {
            self.diff_scroll = line_visual_row;
        } else if line_visual_row >= self.diff_scroll + height {
            self.diff_scroll = line_visual_row.saturating_sub(height.saturating_sub(1));
        }

        if let Some(editor_end) = editor_end
            && editor_end >= self.diff_scroll + height
        {
            self.diff_scroll = editor_end.saturating_sub(height);
        }
    }

    fn selected_file_summary(&self) -> Option<&FileSummary> {
        self.filtered_file_indices
            .get(self.selected_file_view_idx)
            .and_then(|idx| self.files.get(*idx))
    }

    fn current_patch(&self) -> Option<&HighlightedPatch> {
        self.selected_file_summary()
            .and_then(|summary| self.patch_cache.get(&summary.path))
    }

    fn selected_range(&self) -> Option<(usize, usize)> {
        self.selection.map(|selection| selection.normalized())
    }

    fn line_has_comments(&self, line_idx: usize) -> bool {
        !self.comments_on_line(line_idx).is_empty()
    }

    fn comment_ids_on_current_line(&self) -> Vec<u64> {
        self.comments_on_line(self.diff_cursor)
            .iter()
            .map(|annotation| annotation.id)
            .collect()
    }

    fn comments_on_line(&self, line_idx: usize) -> Vec<&Annotation> {
        let Some(summary) = self.selected_file_summary() else {
            return Vec::new();
        };

        self.annotations
            .iter()
            .filter(|annotation| {
                annotation.file_path == summary.path
                    && matches!(
                        annotation.scope,
                        AnnotationScope::Lines {
                            start_line_idx,
                            end_line_idx,
                            ..
                        } if line_idx >= start_line_idx && line_idx <= end_line_idx
                    )
            })
            .collect()
    }

    fn file_comments_for_selected_file(&self) -> Vec<&Annotation> {
        let Some(summary) = self.selected_file_summary() else {
            return Vec::new();
        };

        self.annotations
            .iter()
            .filter(|annotation| annotation.file_path == summary.path && annotation.is_file_level())
            .collect()
    }

    fn file_has_comments(&self, path: &str) -> bool {
        self.annotations
            .iter()
            .any(|annotation| annotation.file_path == path)
    }

    fn set_notification(&mut self, message: String, kind: NotificationKind) {
        self.notification = Some(Notification {
            message,
            created_at: Instant::now(),
            kind,
        });
    }
}

#[derive(Debug, Clone)]
struct DiffItem {
    kind: DiffItemKind,
    height: u16,
}

#[derive(Debug, Clone)]
enum DiffItemKind {
    FileComments,
    Line(usize),
    Editor,
    ExpandedComments { line_idx: usize },
}

fn file_comments_height(comment_count: usize) -> u16 {
    (comment_count as u16 + 2).clamp(3, 6)
}

fn highlight_line_segments(
    syntax_set: &SyntaxSet,
    highlighter: &mut HighlightLines<'_>,
    text: &str,
) -> Vec<StyledSegment> {
    let highlighted = highlighter
        .highlight_line(text, syntax_set)
        .unwrap_or_default();
    if highlighted.is_empty() {
        return vec![StyledSegment {
            text: text.to_string(),
            style: Style::default().fg(Color::White),
        }];
    }

    highlighted
        .into_iter()
        .map(|(style, segment)| StyledSegment {
            text: segment.to_string(),
            style: Style::default().fg(Color::Rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            )),
        })
        .collect()
}

fn change_badge(change: &ChangeKind) -> &'static str {
    match change {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::TypeChange => "T",
        ChangeKind::Copied => "C",
        ChangeKind::Unknown(_) => "?",
    }
}

fn change_color(change: &ChangeKind) -> Color {
    match change {
        ChangeKind::Added => Color::Rgb(149, 198, 136),
        ChangeKind::Modified => Color::Rgb(160, 196, 255),
        ChangeKind::Deleted => Color::Rgb(224, 110, 110),
        ChangeKind::Renamed => Color::Rgb(231, 193, 119),
        ChangeKind::TypeChange => Color::Rgb(214, 180, 255),
        ChangeKind::Copied => Color::Rgb(113, 205, 205),
        ChangeKind::Unknown(_) => Color::Gray,
    }
}

fn diff_sign_color(kind: DiffKind) -> Color {
    match kind {
        DiffKind::Add => Color::Rgb(149, 198, 136),
        DiffKind::Delete => Color::Rgb(224, 110, 110),
        DiffKind::Context => Color::DarkGray,
    }
}

fn diff_base_style(kind: DiffKind) -> Style {
    match kind {
        DiffKind::Add => Style::default().bg(Color::Rgb(18, 40, 26)),
        DiffKind::Delete => Style::default().bg(Color::Rgb(50, 22, 22)),
        DiffKind::Context => Style::default().bg(Color::Rgb(18, 20, 26)),
    }
}

fn panel_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Rgb(160, 196, 255))
    } else {
        Style::default().fg(Color::Rgb(70, 74, 90))
    }
}

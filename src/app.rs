use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
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
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::cli::ReviewArgs;
use crate::clipboard;
use crate::export;
use crate::git::GitRepo;
use crate::model::{
    Annotation, AnnotationLineRange, ChangeKind, DiffKind, FilePatch, FileSummary, Focus,
    LineReference, PatchLine, SelectionRange,
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
struct WholeFileRender {
    lines: Vec<WholeFileLine>,
    hunk_starts: Vec<usize>,
}

#[derive(Debug, Clone)]
struct WholeFileLine {
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
    text: String,
    diff_kind: Option<DiffKind>,
    hunk_header: Option<String>,
}

#[derive(Debug, Clone)]
struct WholeFileHighlightCache {
    lines: HashMap<usize, Vec<StyledSegment>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffViewMode {
    Patch,
    File,
}

struct App {
    repo: GitRepo,
    files: Vec<FileSummary>,
    filtered_file_indices: Vec<usize>,
    selected_file_view_idx: usize,
    file_list_scroll: usize,
    focus: Focus,
    view_mode: DiffViewMode,
    patch_cache: HashMap<String, HighlightedPatch>,
    whole_file_cache: HashMap<String, WholeFileRender>,
    whole_file_highlight_cache: HashMap<String, WholeFileHighlightCache>,
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
    last_files_inner_height: u16,
    last_diff_inner_height: u16,
    syntax_set: SyntaxSet,
    syntax_theme: Theme,
}

impl App {
    fn new(repo: GitRepo, files: Vec<FileSummary>) -> Self {
        let syntax_set = SyntaxSet::load_defaults_nonewlines();
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
            file_list_scroll: 0,
            focus: Focus::Files,
            view_mode: DiffViewMode::Patch,
            patch_cache: HashMap::new(),
            whole_file_cache: HashMap::new(),
            whole_file_highlight_cache: HashMap::new(),
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
            last_files_inner_height: 0,
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
            KeyCode::Char('n') => self.move_file_selection(1),
            KeyCode::Char('p') => self.move_file_selection(-1),
            KeyCode::Char('j') => self.move_down(),
            KeyCode::Char('k') => self.move_up(),
            KeyCode::Char('J') => self.move_hunk(1),
            KeyCode::Char('K') => self.move_hunk(-1),
            KeyCode::Char('g') => self.jump_first(),
            KeyCode::Char('G') => self.jump_last(),
            KeyCode::Char('v') => self.toggle_selection(),
            KeyCode::Char('c') => self.open_comment_draft(),
            KeyCode::Char('C') => self.open_file_comment_draft(),
            KeyCode::Char('t') => self.toggle_view_mode(),
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
            Focus::Files => self.move_file_selection(1),
            Focus::Diff => {
                let line_count = self.current_line_count();
                if line_count == 0 {
                    return;
                }
                if self.diff_cursor + 1 < line_count {
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
            Focus::Files => self.move_file_selection(-1),
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

    fn move_file_selection(&mut self, direction: isize) {
        let file_count = self.filtered_file_indices.len();
        if file_count == 0 {
            return;
        }

        self.selected_file_view_idx = if direction >= 0 {
            if self.selected_file_view_idx + 1 < file_count {
                self.selected_file_view_idx + 1
            } else {
                0
            }
        } else if self.selected_file_view_idx > 0 {
            self.selected_file_view_idx - 1
        } else {
            file_count - 1
        };

        self.reset_diff_view_for_selected_file();
    }

    fn move_hunk(&mut self, direction: isize) {
        let hunk_starts = match self.view_mode {
            DiffViewMode::Patch => self
                .current_patch()
                .map(|patch| patch.hunk_starts.clone())
                .unwrap_or_default(),
            DiffViewMode::File => self
                .current_whole_file()
                .map(|file| file.hunk_starts.clone())
                .unwrap_or_default(),
        };
        if hunk_starts.is_empty() {
            return;
        }

        let mut target = self.diff_cursor;
        if direction > 0 {
            for start in &hunk_starts {
                if *start > self.diff_cursor {
                    target = *start;
                    break;
                }
            }
        } else {
            for start in hunk_starts.iter().rev() {
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
                let line_count = self.current_line_count();
                if line_count > 0 {
                    self.diff_cursor = line_count - 1;
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
        if self.focus != Focus::Diff || self.current_line_count() == 0 {
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
            CommentTarget::Range(range) => match self.view_mode {
                DiffViewMode::Patch => {
                    let patch = self.current_patch().context("missing patch view")?;
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
                DiffViewMode::File => {
                    let file = self
                        .current_whole_file()
                        .context("missing whole-file view")?;
                    let (start, end) = range.normalized();
                    let start_line = file.lines.get(start).context("invalid start line")?;
                    let end_line = file.lines.get(end).context("invalid end line")?;
                    let hunk_header = start_line
                        .hunk_header
                        .clone()
                        .or_else(|| end_line.hunk_header.clone());

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
            },
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

    fn toggle_view_mode(&mut self) {
        let Some(path) = self
            .selected_file_summary()
            .map(|summary| summary.path.clone())
        else {
            return;
        };

        self.selection = None;
        self.comment_draft = None;
        self.expanded_comment_line = None;

        match self.view_mode {
            DiffViewMode::Patch => {
                if !self.load_whole_file_for_selected() {
                    self.set_notification(
                        format!("Whole-file view is unavailable for {path}"),
                        NotificationKind::Error,
                    );
                    return;
                }

                let current_ref = self.current_patch().and_then(|patch| {
                    patch
                        .flat_lines
                        .get(self.diff_cursor)
                        .map(|line| (line.old_lineno, line.new_lineno))
                });
                self.view_mode = DiffViewMode::File;
                if let Some((old_lineno, new_lineno)) = current_ref
                    && let Some(line_idx) = self.find_whole_file_line(old_lineno, new_lineno)
                {
                    self.diff_cursor = line_idx;
                } else {
                    self.diff_cursor = 0;
                }
            }
            DiffViewMode::File => {
                let current_ref = self.current_whole_file().and_then(|file| {
                    file.lines
                        .get(self.diff_cursor)
                        .map(|line| (line.old_lineno, line.new_lineno))
                });
                self.view_mode = DiffViewMode::Patch;
                if let Some((old_lineno, new_lineno)) = current_ref
                    && let Some(line_idx) = self.find_patch_line(old_lineno, new_lineno)
                {
                    self.diff_cursor = line_idx;
                } else {
                    self.diff_cursor = 0;
                }
            }
        }

        self.diff_scroll = 0;
        self.ensure_cursor_visible();
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

    fn render_files(&mut self, frame: &mut Frame, area: Rect) {
        let title = if let Some(filter_input) = &self.filter_input {
            format!(" Files /{} ", filter_input)
        } else if self.filter_query.is_empty() {
            " Files ".to_string()
        } else {
            format!(" Files ({}) ", self.filter_query)
        };

        self.last_files_inner_height = area.height.saturating_sub(2);
        self.ensure_file_selection_visible();

        let visible_rows = self.last_files_inner_height as usize;
        let start_idx = self.file_list_scroll;
        let end_idx = start_idx
            .saturating_add(visible_rows)
            .min(self.filtered_file_indices.len());
        let items: Vec<ListItem> = if self.filtered_file_indices.is_empty() {
            vec![ListItem::new(Line::from("No changed files"))]
        } else {
            self.filtered_file_indices
                .iter()
                .enumerate()
                .skip(start_idx)
                .take(end_idx.saturating_sub(start_idx))
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
            let mode = match self.view_mode {
                DiffViewMode::Patch => "patch",
                DiffViewMode::File => "file",
            };
            format!(" Diff {} [{}] ", summary.path, mode)
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

        match self.view_mode {
            DiffViewMode::Patch => self.render_patch_view(frame, inner),
            DiffViewMode::File => self.render_whole_file_view(frame, inner),
        }
    }

    fn render_patch_view(&mut self, frame: &mut Frame, inner: Rect) {
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

        let items = self.patch_items(patch);
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

    fn render_whole_file_view(&mut self, frame: &mut Frame, inner: Rect) {
        let Some(file_len) = self.current_whole_file().map(|file| file.lines.len()) else {
            let empty = Paragraph::new("Whole-file view is unavailable for this selection.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false });
            frame.render_widget(empty, inner);
            return;
        };

        let layout = self.whole_file_layout();
        let mut y = 0u16;
        let mut visual_offset = 0usize;
        if layout.file_comments_height > 0
            && let Some(row_area) = next_visible_item_area(
                inner,
                &mut y,
                &mut visual_offset,
                self.diff_scroll,
                layout.file_comments_height as u16,
            )
        {
            self.render_file_comments(frame, row_area);
        }
        if layout.file_editor
            && let Some(row_area) = next_visible_item_area(
                inner,
                &mut y,
                &mut visual_offset,
                self.diff_scroll,
                COMMENT_BOX_HEIGHT,
            )
        {
            self.render_comment_editor(frame, row_area);
        }

        let start_line = layout.first_visible_line(self.diff_scroll, file_len);
        visual_offset = layout.line_row_start(start_line);
        for line_idx in start_line..file_len {
            if y >= inner.height {
                break;
            }

            if let Some(row_area) =
                next_visible_item_area(inner, &mut y, &mut visual_offset, self.diff_scroll, 1)
            {
                self.render_whole_file_line(frame, row_area, line_idx);
            }
            if layout.editor_anchor == Some(line_idx)
                && let Some(row_area) = next_visible_item_area(
                    inner,
                    &mut y,
                    &mut visual_offset,
                    self.diff_scroll,
                    COMMENT_BOX_HEIGHT,
                )
            {
                self.render_comment_editor(frame, row_area);
            }
            if layout.expanded_anchor == Some(line_idx)
                && let Some(row_area) = next_visible_item_area(
                    inner,
                    &mut y,
                    &mut visual_offset,
                    self.diff_scroll,
                    COMMENT_BOX_HEIGHT,
                )
            {
                self.render_expanded_comments(frame, row_area, line_idx);
            }
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
            Span::styled(sign, Style::default().fg(diff_sign_color(Some(line.kind)))),
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

    fn render_whole_file_line(&mut self, frame: &mut Frame, area: Rect, line_idx: usize) {
        let Some(line) = self
            .current_whole_file()
            .and_then(|file| file.lines.get(line_idx))
            .cloned()
        else {
            return;
        };
        let in_selection = self
            .selected_range()
            .map(|(start, end)| line_idx >= start && line_idx <= end)
            .unwrap_or(false);
        let has_comments = self.line_has_comments(line_idx);
        let selected = self.focus == Focus::Diff && line_idx == self.diff_cursor;

        let base_style = whole_file_base_style(line.diff_kind);
        let line_style = if selected {
            base_style.bg(Color::Rgb(50, 61, 82))
        } else if in_selection {
            base_style.bg(Color::Rgb(32, 42, 58))
        } else {
            base_style
        };

        let gutter = if has_comments { "●" } else { " " };
        let sign = match line.diff_kind {
            Some(DiffKind::Add) => "+",
            Some(DiffKind::Delete) => "-",
            Some(DiffKind::Context) => "~",
            None => " ",
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
            Span::styled(sign, Style::default().fg(diff_sign_color(line.diff_kind))),
            Span::raw(" "),
        ];

        let highlighted = self.whole_file_highlights(line_idx);
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
            Paragraph::new(comment_editor_line(draft_text))
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
                "h/l focus  j/k move  n/p file  t toggle  v select  c line  C file  Enter inspect  E copy",
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
            self.file_list_scroll = 0;
            self.diff_cursor = 0;
            self.diff_scroll = 0;
            self.selection = None;
            self.comment_draft = None;
            self.expanded_comment_line = None;
        } else if self.selected_file_view_idx >= self.filtered_file_indices.len() {
            self.selected_file_view_idx = self.filtered_file_indices.len() - 1;
            self.ensure_file_selection_visible();
            self.reset_diff_view_for_selected_file();
        } else {
            self.ensure_file_selection_visible();
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
        if self.view_mode == DiffViewMode::File && !self.load_whole_file_for_selected() {
            self.view_mode = DiffViewMode::Patch;
        }
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

    fn load_whole_file_for_selected(&mut self) -> bool {
        let Some(summary) = self.selected_file_summary().cloned() else {
            return false;
        };
        if self.whole_file_cache.contains_key(&summary.path) {
            return true;
        }
        let Some(patch) = self.current_patch() else {
            return false;
        };

        match self.repo.load_file_text(&summary) {
            Ok(Some(text)) => {
                let whole = self.build_whole_file_render(patch, &text);
                self.whole_file_cache.insert(summary.path.clone(), whole);
                self.whole_file_highlight_cache.insert(
                    summary.path.clone(),
                    WholeFileHighlightCache {
                        lines: HashMap::new(),
                    },
                );
                true
            }
            Ok(None) => false,
            Err(err) => {
                self.set_notification(
                    format!("Failed to load file {}: {err}", summary.path),
                    NotificationKind::Error,
                );
                false
            }
        }
    }

    fn highlight_patch(&self, patch: FilePatch) -> HighlightedPatch {
        let syntax = self.syntax_for_path(&patch.summary.path);

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

    fn build_whole_file_render(&self, patch: &HighlightedPatch, text: &str) -> WholeFileRender {
        let mut current_lines: Vec<WholeFileLine> = text
            .lines()
            .enumerate()
            .map(|(idx, line)| WholeFileLine {
                old_lineno: None,
                new_lineno: Some(idx + 1),
                text: line.to_string(),
                diff_kind: None,
                hunk_header: None,
            })
            .collect();

        let mut deleted_blocks: BTreeMap<usize, Vec<WholeFileLine>> = BTreeMap::new();

        for hunk in &patch.patch.hunks {
            let mut pending_deleted = Vec::new();
            let mut last_insert_position = hunk.new_start.saturating_sub(1);

            for line in &hunk.lines {
                match line.kind {
                    DiffKind::Delete => {
                        pending_deleted.push(WholeFileLine {
                            old_lineno: line.old_lineno,
                            new_lineno: None,
                            text: line.text.clone(),
                            diff_kind: Some(DiffKind::Delete),
                            hunk_header: Some(hunk.header.clone()),
                        });
                    }
                    DiffKind::Add | DiffKind::Context => {
                        if let Some(new_lineno) = line.new_lineno {
                            last_insert_position = new_lineno.saturating_sub(1);
                            if let Some(entry) = current_lines.get_mut(new_lineno.saturating_sub(1))
                            {
                                entry.diff_kind = Some(line.kind);
                                entry.hunk_header = Some(hunk.header.clone());
                                entry.old_lineno = line.old_lineno;
                            }
                        }
                        if !pending_deleted.is_empty() {
                            deleted_blocks
                                .entry(last_insert_position)
                                .or_default()
                                .append(&mut pending_deleted);
                        }
                    }
                }
            }

            if !pending_deleted.is_empty() {
                let position = if patch.patch.summary.change == ChangeKind::Deleted {
                    0
                } else {
                    let hunk_end = hunk.new_start.saturating_sub(1)
                        + hunk.lines.iter().filter_map(|line| line.new_lineno).count();
                    hunk_end.min(current_lines.len())
                };
                deleted_blocks
                    .entry(position)
                    .or_default()
                    .append(&mut pending_deleted);
            }
        }

        let mut lines = Vec::new();
        for idx in 0..=current_lines.len() {
            if let Some(mut deleted) = deleted_blocks.remove(&idx) {
                lines.append(&mut deleted);
            }
            if let Some(line) = current_lines.get(idx) {
                lines.push(line.clone());
            }
        }

        let mut hunk_starts = Vec::new();
        let mut last_hunk_header: Option<String> = None;
        for (idx, line) in lines.iter().enumerate() {
            if let Some(header) = &line.hunk_header
                && last_hunk_header.as_ref() != Some(header)
            {
                hunk_starts.push(idx);
                last_hunk_header = Some(header.clone());
            } else if line.hunk_header.is_none() {
                last_hunk_header = None;
            }
        }

        WholeFileRender { lines, hunk_starts }
    }

    fn patch_items(&self, patch: &HighlightedPatch) -> Vec<DiffItem> {
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

        let (line_visual_row, editor_end) = match self.view_mode {
            DiffViewMode::Patch => {
                let items = self
                    .current_patch()
                    .map(|patch| self.patch_items(patch))
                    .unwrap_or_default();
                if items.is_empty() {
                    return;
                }
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

                (line_visual_row, editor_end)
            }
            DiffViewMode::File => {
                let Some(file_len) = self.current_whole_file().map(|file| file.lines.len()) else {
                    return;
                };
                if file_len == 0 {
                    return;
                }
                let layout = self.whole_file_layout();
                let line_visual_row = layout.line_row_start(self.diff_cursor.min(file_len - 1));
                let editor_end = if let Some(draft) = &self.comment_draft {
                    match draft.target {
                        CommentTarget::File => {
                            Some(layout.file_comments_height + COMMENT_BOX_HEIGHT as usize)
                        }
                        CommentTarget::Range(range) => Some(
                            layout.editor_start(range.normalized().1) + COMMENT_BOX_HEIGHT as usize,
                        ),
                    }
                } else {
                    layout.expanded_anchor.map(|line_idx| {
                        layout.expanded_start(line_idx) + COMMENT_BOX_HEIGHT as usize
                    })
                };

                (line_visual_row, editor_end)
            }
        };

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

    fn current_line_count(&self) -> usize {
        match self.view_mode {
            DiffViewMode::Patch => self
                .current_patch()
                .map(|patch| patch.flat_lines.len())
                .unwrap_or(0),
            DiffViewMode::File => self
                .current_whole_file()
                .map(|file| file.lines.len())
                .unwrap_or(0),
        }
    }

    fn current_patch(&self) -> Option<&HighlightedPatch> {
        self.selected_file_summary()
            .and_then(|summary| self.patch_cache.get(&summary.path))
    }

    fn current_whole_file(&self) -> Option<&WholeFileRender> {
        self.selected_file_summary()
            .and_then(|summary| self.whole_file_cache.get(&summary.path))
    }

    fn ensure_file_selection_visible(&mut self) {
        let total_items = self.filtered_file_indices.len();
        let visible_rows = self.last_files_inner_height as usize;
        if total_items == 0 || visible_rows == 0 {
            self.file_list_scroll = 0;
            return;
        }

        let max_scroll = total_items.saturating_sub(visible_rows);
        self.file_list_scroll = self.file_list_scroll.min(max_scroll);

        if self.selected_file_view_idx < self.file_list_scroll {
            self.file_list_scroll = self.selected_file_view_idx;
        } else if self.selected_file_view_idx >= self.file_list_scroll + visible_rows {
            self.file_list_scroll = self
                .selected_file_view_idx
                .saturating_sub(visible_rows.saturating_sub(1))
                .min(max_scroll);
        }
    }

    fn whole_file_layout(&self) -> WholeFileLayout {
        let file_comments_count = self.file_comments_for_selected_file().len();
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

        WholeFileLayout {
            file_comments_height: if file_comments_count > 0 {
                file_comments_height(file_comments_count) as usize
            } else {
                0
            },
            file_editor,
            editor_anchor,
            expanded_anchor: self.expanded_comment_line,
        }
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
        let line_ref = match self.view_mode {
            DiffViewMode::Patch => self.current_patch().and_then(|patch| {
                patch
                    .flat_lines
                    .get(line_idx)
                    .map(|line| (line.old_lineno, line.new_lineno))
            }),
            DiffViewMode::File => self.current_whole_file().and_then(|file| {
                file.lines
                    .get(line_idx)
                    .map(|line| (line.old_lineno, line.new_lineno))
            }),
        };

        let Some((old_lineno, new_lineno)) = line_ref else {
            return Vec::new();
        };

        let Some(summary) = self.selected_file_summary() else {
            return Vec::new();
        };

        self.annotations
            .iter()
            .filter(|annotation| annotation.file_path == summary.path)
            .filter(|annotation| self.annotation_matches_line(annotation, old_lineno, new_lineno))
            .collect()
    }

    fn annotation_matches_line(
        &self,
        annotation: &Annotation,
        old_lineno: Option<usize>,
        new_lineno: Option<usize>,
    ) -> bool {
        let Some((start_ref, end_ref)) = annotation.line_refs() else {
            return false;
        };

        let old_match = match (old_lineno, start_ref.old_lineno, end_ref.old_lineno) {
            (Some(line), Some(start), Some(end)) => {
                line >= start.min(end) && line <= start.max(end)
            }
            _ => false,
        };
        let new_match = match (new_lineno, start_ref.new_lineno, end_ref.new_lineno) {
            (Some(line), Some(start), Some(end)) => {
                line >= start.min(end) && line <= start.max(end)
            }
            _ => false,
        };

        old_match || new_match
    }

    fn find_whole_file_line(
        &self,
        old_lineno: Option<usize>,
        new_lineno: Option<usize>,
    ) -> Option<usize> {
        self.current_whole_file().and_then(|file| {
            file.lines.iter().position(|line| {
                if let Some(new_lineno) = new_lineno {
                    line.new_lineno == Some(new_lineno)
                        && (old_lineno.is_none() || line.old_lineno == old_lineno)
                } else {
                    old_lineno.is_some() && line.old_lineno == old_lineno
                }
            })
        })
    }

    fn find_patch_line(
        &self,
        old_lineno: Option<usize>,
        new_lineno: Option<usize>,
    ) -> Option<usize> {
        self.current_patch().and_then(|patch| {
            patch.flat_lines.iter().position(|line| {
                if let Some(new_lineno) = new_lineno {
                    line.new_lineno == Some(new_lineno)
                        && (old_lineno.is_none() || line.old_lineno == old_lineno)
                } else {
                    old_lineno.is_some() && line.old_lineno == old_lineno
                }
            })
        })
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

    fn syntax_for_path(&self, path: &str) -> &SyntaxReference {
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| {
                self.syntax_set
                    .find_syntax_by_extension(extension)
                    .or_else(|| {
                        syntax_extension_aliases(extension)
                            .iter()
                            .copied()
                            .find_map(|alias| self.syntax_set.find_syntax_by_extension(alias))
                    })
            })
            .or_else(|| {
                self.syntax_set
                    .find_syntax_for_file(self.repo.root.join(path))
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }

    fn whole_file_highlights(&mut self, line_idx: usize) -> Vec<StyledSegment> {
        let Some(summary) = self.selected_file_summary() else {
            return Vec::new();
        };
        let path = summary.path.clone();
        let Some(file) = self.whole_file_cache.get(&path) else {
            return Vec::new();
        };
        if line_idx >= file.lines.len() {
            return Vec::new();
        }

        if let Some(segments) = self
            .whole_file_highlight_cache
            .get(&path)
            .and_then(|cache| cache.lines.get(&line_idx))
            .cloned()
        {
            return segments;
        }

        let Some(line) = file.lines.get(line_idx) else {
            return Vec::new();
        };
        let mut highlighter = HighlightLines::new(self.syntax_for_path(&path), &self.syntax_theme);
        let segments = highlight_line_segments(&self.syntax_set, &mut highlighter, &line.text);

        if let Some(cache) = self.whole_file_highlight_cache.get_mut(&path) {
            cache.lines.insert(line_idx, segments.clone());
        }

        segments
    }
}

#[derive(Debug, Clone)]
struct DiffItem {
    kind: DiffItemKind,
    height: u16,
}

#[derive(Debug, Clone, Copy)]
struct WholeFileLayout {
    file_comments_height: usize,
    file_editor: bool,
    editor_anchor: Option<usize>,
    expanded_anchor: Option<usize>,
}

impl WholeFileLayout {
    fn prefix_rows(self) -> usize {
        self.file_comments_height
            + if self.file_editor {
                COMMENT_BOX_HEIGHT as usize
            } else {
                0
            }
    }

    fn line_row_start(self, line_idx: usize) -> usize {
        self.prefix_rows() + line_idx + self.extra_rows_before_line(line_idx)
    }

    fn editor_start(self, line_idx: usize) -> usize {
        self.line_row_start(line_idx) + 1
    }

    fn expanded_start(self, line_idx: usize) -> usize {
        self.editor_start(line_idx)
            + if self.editor_anchor == Some(line_idx) {
                COMMENT_BOX_HEIGHT as usize
            } else {
                0
            }
    }

    fn first_visible_line(self, diff_scroll: usize, file_line_count: usize) -> usize {
        if file_line_count == 0 || diff_scroll <= self.prefix_rows() {
            return 0;
        }

        let mut line_idx = diff_scroll
            .saturating_sub(self.prefix_rows())
            .min(file_line_count - 1);

        for _ in 0..3 {
            let adjusted = diff_scroll
                .saturating_sub(self.prefix_rows() + self.extra_rows_before_line(line_idx))
                .min(file_line_count - 1);
            if adjusted == line_idx {
                break;
            }
            line_idx = adjusted;
        }

        while line_idx > 0 && self.line_row_start(line_idx) > diff_scroll {
            line_idx -= 1;
        }
        while line_idx + 1 < file_line_count && self.line_row_start(line_idx + 1) <= diff_scroll {
            line_idx += 1;
        }

        line_idx
    }

    fn extra_rows_before_line(self, line_idx: usize) -> usize {
        let editor_rows = if self.editor_anchor.is_some_and(|anchor| anchor < line_idx) {
            COMMENT_BOX_HEIGHT as usize
        } else {
            0
        };
        let expanded_rows = if self.expanded_anchor.is_some_and(|anchor| anchor < line_idx) {
            COMMENT_BOX_HEIGHT as usize
        } else {
            0
        };
        editor_rows + expanded_rows
    }
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

fn next_visible_item_area(
    inner: Rect,
    y: &mut u16,
    visual_offset: &mut usize,
    diff_scroll: usize,
    item_height: u16,
) -> Option<Rect> {
    let current_offset = *visual_offset;
    *visual_offset += item_height as usize;

    if current_offset + item_height as usize <= diff_scroll || *y >= inner.height {
        return None;
    }

    let skip_rows = diff_scroll.saturating_sub(current_offset);
    let available_height = inner.height.saturating_sub(*y);
    let render_height = item_height
        .saturating_sub(skip_rows as u16)
        .min(available_height);
    if render_height == 0 {
        return None;
    }

    let row_area = Rect {
        x: inner.x,
        y: inner.y + *y,
        width: inner.width,
        height: render_height,
    };
    *y += render_height;
    Some(row_area)
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

fn diff_sign_color(kind: Option<DiffKind>) -> Color {
    match kind {
        Some(DiffKind::Add) => Color::Rgb(149, 198, 136),
        Some(DiffKind::Delete) => Color::Rgb(224, 110, 110),
        Some(DiffKind::Context) => Color::Rgb(133, 146, 178),
        None => Color::DarkGray,
    }
}

fn diff_base_style(kind: DiffKind) -> Style {
    match kind {
        DiffKind::Add => Style::default().bg(Color::Rgb(18, 40, 26)),
        DiffKind::Delete => Style::default().bg(Color::Rgb(50, 22, 22)),
        DiffKind::Context => Style::default().bg(Color::Rgb(18, 20, 26)),
    }
}

fn whole_file_base_style(kind: Option<DiffKind>) -> Style {
    match kind {
        Some(kind) => diff_base_style(kind),
        None => Style::default().bg(Color::Rgb(15, 17, 22)),
    }
}

fn panel_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Rgb(160, 196, 255))
    } else {
        Style::default().fg(Color::Rgb(70, 74, 90))
    }
}

fn comment_editor_line(draft_text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    if !draft_text.is_empty() {
        spans.push(Span::raw(draft_text.to_string()));
    }
    spans.push(Span::styled(
        "▊",
        Style::default().fg(Color::Rgb(216, 180, 84)),
    ));
    Line::from(spans)
}

fn syntax_extension_aliases(extension: &str) -> &'static [&'static str] {
    match extension {
        "ts" | "mts" | "cts" => &["js"],
        "tsx" => &["jsx", "js"],
        "mjs" | "cjs" => &["js"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("rebyua-tests-{unique}"));
            fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_theme() -> Theme {
        let theme_set = ThemeSet::load_defaults();
        theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| theme_set.themes.values().next().cloned())
            .unwrap_or_default()
    }

    fn file_summary(path: &str) -> FileSummary {
        FileSummary {
            path: path.to_string(),
            old_path: None,
            added: Some(1),
            deleted: Some(1),
            change: ChangeKind::Modified,
        }
    }

    fn sample_patch(summary: &FileSummary) -> FilePatch {
        FilePatch {
            summary: summary.clone(),
            metadata: Vec::new(),
            hunks: vec![crate::model::PatchHunk {
                header: "@@ -1,4 +1,4 @@ fn main() {".to_string(),
                new_start: 1,
                lines: vec![
                    PatchLine {
                        kind: DiffKind::Context,
                        old_lineno: Some(1),
                        new_lineno: Some(1),
                        text: "fn main() {".to_string(),
                    },
                    PatchLine {
                        kind: DiffKind::Delete,
                        old_lineno: Some(2),
                        new_lineno: None,
                        text: "    let value = 1;".to_string(),
                    },
                    PatchLine {
                        kind: DiffKind::Add,
                        old_lineno: None,
                        new_lineno: Some(2),
                        text: "    let value = 2;".to_string(),
                    },
                    PatchLine {
                        kind: DiffKind::Context,
                        old_lineno: Some(3),
                        new_lineno: Some(3),
                        text: "    println!(\"{}\", value);".to_string(),
                    },
                    PatchLine {
                        kind: DiffKind::Context,
                        old_lineno: Some(4),
                        new_lineno: Some(4),
                        text: "}".to_string(),
                    },
                ],
            }],
        }
    }

    fn sample_file_text() -> &'static str {
        "fn main() {\n    let value = 2;\n    println!(\"{}\", value);\n}\n"
    }

    fn test_app(root: PathBuf, files: Vec<FileSummary>) -> App {
        App {
            repo: GitRepo {
                root,
                base: "HEAD".to_string(),
                staged: false,
                pathspecs: Vec::new(),
            },
            files,
            filtered_file_indices: vec![0],
            selected_file_view_idx: 0,
            file_list_scroll: 0,
            focus: Focus::Diff,
            view_mode: DiffViewMode::Patch,
            patch_cache: HashMap::new(),
            whole_file_cache: HashMap::new(),
            whole_file_highlight_cache: HashMap::new(),
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
            last_files_inner_height: 0,
            last_diff_inner_height: 20,
            syntax_set: SyntaxSet::load_defaults_nonewlines(),
            syntax_theme: test_theme(),
        }
    }

    fn seed_patch(app: &mut App) {
        let summary = app
            .selected_file_summary()
            .expect("selected file should exist")
            .clone();
        let patch = app.highlight_patch(sample_patch(&summary));
        app.patch_cache.insert(summary.path.clone(), patch);
    }

    fn key_char(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_lock_stops_following_cursor() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.rs");
        let mut app = test_app(temp.path.clone(), vec![summary]);
        seed_patch(&mut app);
        app.diff_cursor = 1;

        app.on_key(key_char('v')).expect("selection should start");
        assert_eq!(
            app.selection
                .map(|selection| (selection.anchor, selection.cursor, selection.locked)),
            Some((1, 1, false))
        );

        app.on_key(key_char('j'))
            .expect("cursor should move while selecting");
        assert_eq!(app.diff_cursor, 2);
        assert_eq!(
            app.selection
                .map(|selection| (selection.anchor, selection.cursor, selection.locked)),
            Some((1, 2, false))
        );

        app.on_key(key_char('v')).expect("selection should lock");
        assert_eq!(
            app.selection
                .map(|selection| (selection.anchor, selection.cursor, selection.locked)),
            Some((1, 2, true))
        );

        app.on_key(key_char('j'))
            .expect("cursor should still move after locking");
        assert_eq!(app.diff_cursor, 3);
        assert_eq!(
            app.selection
                .map(|selection| (selection.anchor, selection.cursor, selection.locked)),
            Some((1, 2, true))
        );
    }

    #[test]
    fn saves_line_comment_from_current_selection() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.rs");
        let mut app = test_app(temp.path.clone(), vec![summary.clone()]);
        seed_patch(&mut app);
        app.diff_cursor = 2;

        app.on_key(key_char('c'))
            .expect("line comment draft should open");
        for ch in "tighten branch".chars() {
            app.on_key(key_char(ch))
                .expect("comment draft input should be accepted");
        }
        app.on_key(key(KeyCode::Enter))
            .expect("comment should be saved");

        assert!(app.comment_draft.is_none());
        assert_eq!(app.annotations.len(), 1);
        let annotation = &app.annotations[0];
        assert_eq!(annotation.file_path, summary.path);
        assert_eq!(annotation.body, "tighten branch");
        assert_eq!(annotation.line_range(), Some((2, 2)));
        let (start_ref, end_ref) = annotation.line_refs().expect("line refs should exist");
        assert_eq!(start_ref.old_lineno, None);
        assert_eq!(start_ref.new_lineno, Some(2));
        assert_eq!(end_ref.old_lineno, None);
        assert_eq!(end_ref.new_lineno, Some(2));
    }

    #[test]
    fn saves_file_level_comment() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.rs");
        let mut app = test_app(temp.path.clone(), vec![summary.clone()]);
        seed_patch(&mut app);

        app.on_key(key_char('C'))
            .expect("file comment draft should open");
        for ch in "needs broader cleanup".chars() {
            app.on_key(key_char(ch))
                .expect("file comment input should be accepted");
        }
        app.on_key(key(KeyCode::Enter))
            .expect("file comment should save");

        assert_eq!(app.annotations.len(), 1);
        let annotation = &app.annotations[0];
        assert_eq!(annotation.file_path, summary.path);
        assert!(annotation.is_file_level());
        assert_eq!(annotation.body, "needs broader cleanup");
    }

    #[test]
    fn quit_requires_confirmation_when_comments_exist() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.rs");
        let mut app = test_app(temp.path.clone(), vec![summary.clone()]);
        seed_patch(&mut app);
        app.annotations.push(Annotation::created_for_file(
            1,
            summary.path,
            "note".to_string(),
        ));

        app.on_key(key_char('q'))
            .expect("first quit should warn instead of exiting");
        assert!(app.pending_quit_confirmation);
        assert!(!app.should_quit);

        app.on_key(key_char('j'))
            .expect("normal input should clear pending confirmation");
        assert!(!app.pending_quit_confirmation);

        app.on_key(key_char('q'))
            .expect("warning should reappear on next quit");
        app.on_key(key_char('q')).expect("second quit should exit");
        assert!(app.should_quit);
    }

    #[test]
    fn toggle_view_mode_preserves_cursor_location_between_patch_and_file_views() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.rs");
        let full_path = temp.path.join(&summary.path);
        fs::create_dir_all(full_path.parent().expect("parent should exist"))
            .expect("parent directories should be created");
        fs::write(&full_path, sample_file_text()).expect("sample file should be written");

        let mut app = test_app(temp.path.clone(), vec![summary.clone()]);
        seed_patch(&mut app);
        app.diff_cursor = 2;

        app.on_key(key_char('t'))
            .expect("toggle to whole-file view should succeed");
        assert_eq!(app.view_mode, DiffViewMode::File);
        assert_eq!(app.diff_cursor, 2);

        let line = &app
            .current_whole_file()
            .expect("whole-file view should be cached")
            .lines[app.diff_cursor];
        assert_eq!(line.old_lineno, None);
        assert_eq!(line.new_lineno, Some(2));
        assert_eq!(line.text, "    let value = 2;");

        app.on_key(key_char('t'))
            .expect("toggle back to patch view should succeed");
        assert_eq!(app.view_mode, DiffViewMode::Patch);
        assert_eq!(app.diff_cursor, 2);
    }

    #[test]
    fn file_list_navigation_wraps_at_both_ends() {
        let temp = TempDirGuard::new();
        let files = vec![
            file_summary("src/one.rs"),
            file_summary("src/two.rs"),
            file_summary("src/three.rs"),
        ];
        let mut app = test_app(temp.path.clone(), files);
        app.focus = Focus::Files;
        app.filtered_file_indices = vec![0, 1, 2];
        app.selected_file_view_idx = 2;

        app.on_key(key_char('j'))
            .expect("moving down from the last file should wrap");
        assert_eq!(app.selected_file_view_idx, 0);

        app.on_key(key_char('k'))
            .expect("moving up from the first file should wrap");
        assert_eq!(app.selected_file_view_idx, 2);
    }

    #[test]
    fn next_and_previous_file_hotkeys_work_from_diff_focus() {
        let temp = TempDirGuard::new();
        let files = vec![
            file_summary("src/one.rs"),
            file_summary("src/two.rs"),
            file_summary("src/three.rs"),
        ];
        let mut app = test_app(temp.path.clone(), files);
        app.focus = Focus::Diff;
        app.filtered_file_indices = vec![0, 1, 2];
        app.selected_file_view_idx = 0;

        app.on_key(key_char('n'))
            .expect("n should move to the next file");
        assert_eq!(app.selected_file_view_idx, 1);

        app.on_key(key_char('p'))
            .expect("p should move to the previous file");
        assert_eq!(app.selected_file_view_idx, 0);

        app.on_key(key_char('p'))
            .expect("p should wrap to the last file");
        assert_eq!(app.selected_file_view_idx, 2);
    }

    #[test]
    fn highlights_typescript_tokens_with_multiple_colors() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/demo.ts");
        let app = test_app(temp.path.clone(), vec![summary.clone()]);
        let patch = app.highlight_patch(FilePatch {
            summary,
            metadata: Vec::new(),
            hunks: vec![crate::model::PatchHunk {
                header: "@@ -1 +1 @@".to_string(),
                new_start: 1,
                lines: vec![PatchLine {
                    kind: DiffKind::Context,
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                    text: "import { describe } from \"vitest\";".to_string(),
                }],
            }],
        });

        let line_highlights = patch
            .highlights
            .first()
            .expect("highlighted line should exist");
        let unique_colors: std::collections::HashSet<_> = line_highlights
            .iter()
            .map(|segment| segment.style.fg)
            .collect();

        assert!(
            unique_colors.len() > 1,
            "expected syntax highlighting to use multiple colors, got {unique_colors:?}"
        );
    }

    #[test]
    fn comment_editor_line_appends_visible_cursor() {
        let line = comment_editor_line("review note");

        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "review note");
        assert_eq!(line.spans[1].content.as_ref(), "▊");
        assert_eq!(line.spans[1].style.fg, Some(Color::Rgb(216, 180, 84)));
    }

    #[test]
    fn file_list_scroll_keeps_selection_visible_without_edge_pinning() {
        let temp = TempDirGuard::new();
        let files = (0..10)
            .map(|idx| file_summary(&format!("src/{idx}.rs")))
            .collect();
        let mut app = test_app(temp.path.clone(), files);
        app.filtered_file_indices = (0..10).collect();
        app.last_files_inner_height = 4;

        app.selected_file_view_idx = 4;
        app.ensure_file_selection_visible();
        assert_eq!(app.file_list_scroll, 1);

        app.selected_file_view_idx = 3;
        app.ensure_file_selection_visible();
        assert_eq!(app.file_list_scroll, 1);

        app.selected_file_view_idx = 1;
        app.ensure_file_selection_visible();
        assert_eq!(app.file_list_scroll, 1);

        app.selected_file_view_idx = 0;
        app.ensure_file_selection_visible();
        assert_eq!(app.file_list_scroll, 0);
    }

    #[test]
    #[ignore = "profiling helper"]
    fn profile_whole_file_render_path() {
        let temp = TempDirGuard::new();
        let summary = file_summary("src/huge.ts");
        let mut app = test_app(temp.path.clone(), vec![summary.clone()]);
        app.view_mode = DiffViewMode::File;

        let line_count = 50_000usize;
        let lines: Vec<WholeFileLine> = (0..line_count)
            .map(|idx| WholeFileLine {
                old_lineno: Some(idx + 1),
                new_lineno: Some(idx + 1),
                text: format!("const value{idx} = value{idx} + 1;"),
                diff_kind: if idx % 200 == 0 {
                    Some(DiffKind::Add)
                } else {
                    None
                },
                hunk_header: if idx % 200 == 0 {
                    Some(format!("@@ -{},1 +{},1 @@", idx + 1, idx + 1))
                } else {
                    None
                },
            })
            .collect();
        let hunk_starts = (0..line_count).step_by(200).collect();
        app.whole_file_cache
            .insert(summary.path.clone(), WholeFileRender { lines, hunk_starts });
        app.whole_file_highlight_cache.insert(
            summary.path.clone(),
            WholeFileHighlightCache {
                lines: HashMap::new(),
            },
        );

        let mut terminal =
            Terminal::new(TestBackend::new(140, 40)).expect("test terminal should initialize");

        app.diff_scroll = 0;
        let start = Instant::now();
        for _ in 0..25 {
            terminal
                .draw(|frame| app.render(frame))
                .expect("render should succeed");
        }
        let top_render_elapsed = start.elapsed();

        app.diff_cursor = line_count / 2;
        app.diff_scroll = line_count / 2;
        let start = Instant::now();
        for _ in 0..25 {
            terminal
                .draw(|frame| app.render(frame))
                .expect("render should succeed");
        }
        let deep_render_elapsed = start.elapsed();

        let start = Instant::now();
        for _ in 0..1_000 {
            app.ensure_cursor_visible();
        }
        let cursor_elapsed = start.elapsed();

        println!("whole-file top render(25x, {line_count} lines): {top_render_elapsed:?}");
        println!("whole-file deep render(25x, {line_count} lines): {deep_render_elapsed:?}");
        println!("ensure_cursor_visible(1000x): {cursor_elapsed:?}");
    }
}

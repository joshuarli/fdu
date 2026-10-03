use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use fdu_core::{EntryType, NodeId, NodeState, Tree};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

pub enum Intent {
    Quit,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Parent,
    ToggleMark,
    ToggleRange,
    MarkAll,
    ClearMarks,
    Delete,
    Refresh,
    ToggleSizeMode,
    SortByName,
    SortBySize,
    StartFilter,
    FilterCharacter(char),
    FilterBackspace,
    SubmitFilter,
    Cancel,
    Help,
}

#[derive(Clone, Copy)]
pub enum Cursor {
    Parent,
    Entry(NodeId),
    None,
}

#[derive(Clone, Copy)]
pub enum SizeMode {
    Allocated,
    Apparent,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SortMode {
    Size,
    Name,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Phase {
    Scanning,
    Ready,
    Deleting,
}

pub enum Modal<'a> {
    None,
    Help,
    Filter { value: &'a str },
    ConfirmDelete {
        roots: &'a [NodeId],
        descendants: usize,
        apparent_bytes: u64,
        allocated_bytes: u64,
        rejected: &'a [String],
        eligible: bool,
    },
    Deleting {
        attempted: usize,
        total: usize,
        current: &'a str,
        cancellable: bool,
    },
}

pub struct View<'a> {
    pub tree: &'a Tree,
    pub current_directory: NodeId,
    pub rows: &'a [NodeId],
    pub has_parent_row: bool,
    pub cursor_index: Option<usize>,
    pub marks: &'a HashSet<NodeId>,
    pub filter: &'a str,
    pub size_mode: SizeMode,
    pub sort_mode: SortMode,
    pub phase: Phase,
    pub indexed_entries: usize,
    pub directory_items: usize,
    pub visible_items: usize,
    pub incomplete_entries: usize,
    pub exclusions: usize,
    pub read_only: bool,
    pub range_mode: bool,
    pub message: Option<&'a str>,
    pub detail_message: Option<&'a str>,
    pub modal: Modal<'a>,
}

pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        install_panic_restore();
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                active: true,
            }),
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    pub fn draw(&mut self, view: &View<'_>) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, view)).map(|_| ())
    }

    pub fn restore(&mut self) {
        if self.active {
            self.active = false;
            restore_terminal();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn poll_intent(timeout: Duration, text_input: bool) -> io::Result<Option<Intent>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    let Event::Key(key) = event::read()? else {
        return Ok(None);
    };
    Ok(map_key(key, text_input))
}

fn map_key(key: KeyEvent, text_input: bool) -> Option<Intent> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if text_input {
        return match key.code {
            KeyCode::Esc => Some(Intent::Cancel),
            KeyCode::Enter => Some(Intent::SubmitFilter),
            KeyCode::Backspace => Some(Intent::FilterBackspace),
            KeyCode::Char(value) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Intent::FilterCharacter(value))
            }
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('c') if control => Some(Intent::Quit),
        KeyCode::Up | KeyCode::Char('k') => Some(Intent::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(Intent::MoveDown),
        KeyCode::PageUp => Some(Intent::PageUp),
        KeyCode::PageDown => Some(Intent::PageDown),
        KeyCode::Home => Some(Intent::Home),
        KeyCode::End => Some(Intent::End),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Some(Intent::Enter),
        KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => Some(Intent::Parent),
        KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(Intent::ToggleRange)
        }
        KeyCode::Char(' ') => Some(Intent::ToggleMark),
        KeyCode::Char('v') => Some(Intent::ToggleRange),
        KeyCode::Char('*') => Some(Intent::MarkAll),
        KeyCode::Char('c') => Some(Intent::ClearMarks),
        KeyCode::Char('d') => Some(Intent::Delete),
        KeyCode::Char('r') => Some(Intent::Refresh),
        KeyCode::Char('a') => Some(Intent::ToggleSizeMode),
        KeyCode::Char('n') => Some(Intent::SortByName),
        KeyCode::Char('s') => Some(Intent::SortBySize),
        KeyCode::Char('/') => Some(Intent::StartFilter),
        KeyCode::Char('?') => Some(Intent::Help),
        KeyCode::Esc => Some(Intent::Cancel),
        KeyCode::Char('q') => Some(Intent::Quit),
        KeyCode::Char(value) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
            Some(Intent::FilterCharacter(value))
        }
        _ => None,
    }
}

fn render(frame: &mut Frame<'_>, view: &View<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(frame.area());
    let header = header_text(view);
    frame.render_widget(Paragraph::new(header).block(Block::default().borders(Borders::BOTTOM)), chunks[0]);

    let list_area = chunks[1];
    render_rows(frame, list_area, view);

    let footer = footer_text(view);
    frame.render_widget(Paragraph::new(footer).block(Block::default().borders(Borders::TOP)), chunks[2]);
    render_modal(frame, view);
}

fn header_text(view: &View<'_>) -> Vec<Line<'static>> {
    let mode = match view.size_mode {
        SizeMode::Allocated => "allocated",
        SizeMode::Apparent => "apparent",
    };
    let sort = match view.sort_mode {
        SortMode::Size => "size",
        SortMode::Name => "name",
    };
    let phase = match view.phase {
        Phase::Scanning => format!(
            "Scanning · {} indexed · {}/{} shown",
            view.indexed_entries, view.visible_items, view.directory_items
        ),
        Phase::Ready => format!("Ready · {}/{} shown", view.visible_items, view.directory_items),
        Phase::Deleting => format!("Deleting · {}/{} shown", view.visible_items, view.directory_items),
    };
    let mut path = view
        .tree
        .materialize_path(view.current_directory)
        .unwrap_or_default()
        .into_iter()
        .map(escape_name)
        .collect::<Vec<_>>();
    let root_name = view.tree.name(view.tree.root()).unwrap_or_default();
    let mut breadcrumb = escape_name(root_name);
    for component in path.drain(..) {
        breadcrumb.push('/');
        breadcrumb.push_str(&component);
    }
    let phase_style = match view.phase {
        Phase::Scanning => Style::default().fg(Color::Yellow),
        Phase::Ready => Style::default().fg(Color::Green),
        Phase::Deleting => Style::default().fg(Color::Red),
    };
    vec![
        Line::from(vec![
            Span::styled("fdu ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(phase, phase_style),
            Span::raw(format!(" · {mode} · sort {sort} · marks {}", view.marks.len())),
        ]),
        Line::from(breadcrumb),
    ]
}

fn render_rows(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let parent_offset = usize::from(view.has_parent_row);
    let cursor_index = view.cursor_index;
    let available_rows = usize::from(area.height);
    let row_count = view.rows.len() + parent_offset;
    let start = cursor_index
        .filter(|index| *index < row_count)
        .map(|index| index.saturating_sub(available_rows.saturating_sub(1)))
        .unwrap_or(0)
        .min(row_count.saturating_sub(available_rows));
    let visible_end = start.saturating_add(available_rows).min(row_count);
    let max_size = (start..visible_end)
        .filter_map(|index| row_record(view, index, parent_offset).map(|record| size_of(record, view.size_mode)))
        .max()
        .unwrap_or(0);

    for screen_row in 0..available_rows {
        let index = start + screen_row;
        if index >= visible_end {
            break;
        }
        let y = area.y.saturating_add(screen_row as u16);
        let row_area = Rect::new(area.x, y, area.width, 1);
        let is_selected = cursor_index == Some(index);
        if index < parent_offset {
            let style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            frame.render_widget(Paragraph::new("  ../").style(style), row_area);
            continue;
        }
        let Some(id) = view.rows.get(index - parent_offset).copied() else {
            continue;
        };
        let Some(record) = view.tree.record(id) else {
            continue;
        };
        let selected = is_selected;
        let state_marker = match record.state {
            NodeState::Scanning => "…",
            NodeState::Incomplete => "!",
            NodeState::Excluded(fdu_core::ExclusionReason::MountBoundary) => "M",
            NodeState::Excluded(fdu_core::ExclusionReason::UnsupportedAlias) => "A",
            NodeState::Stale => "~",
            NodeState::Tombstone => " ",
            NodeState::Complete => " ",
        };
        let marker_style = match record.state {
            NodeState::Incomplete | NodeState::Stale => Style::default().fg(Color::Yellow),
            NodeState::Excluded(_) => Style::default().fg(Color::Red),
            NodeState::Scanning => Style::default().fg(Color::Cyan),
            _ => Style::default(),
        };
        let marked = view.marks.contains(&id);
        let entry_name = escape_name(view.tree.name(id).unwrap_or_default());
        let suffix = if record.entry_type == EntryType::Directory { "/" } else { "" };
        let link_marker = if record.link_count > 1 && record.entry_type != EntryType::Directory { "*" } else { " " };
        let value = size_of(record, view.size_mode);
        let size_text = match record.state {
            NodeState::Excluded(fdu_core::ExclusionReason::MountBoundary) => "mount".to_owned(),
            NodeState::Excluded(fdu_core::ExclusionReason::UnsupportedAlias) => "alias".to_owned(),
            NodeState::Scanning if record.entry_type == EntryType::Directory => "…".to_owned(),
            NodeState::Incomplete => format!("~{}", format_size(value)),
            NodeState::Stale => format!("stale {}", format_size(value)),
            _ => format_size(size_of(record, view.size_mode)),
        };
        let bar_width = area.width.min(10).saturating_sub(1) as usize;
        let bar_count = if max_size == 0 {
            0
        } else {
            ((u128::from(value) * bar_width as u128) / u128::from(max_size)) as usize
        };
        let mut bar = String::with_capacity(bar_width);
        bar.extend(std::iter::repeat('█').take(bar_count.min(bar_width)));
        bar.extend(std::iter::repeat(' ').take(bar_width.saturating_sub(bar_count)));
        let selected_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        let mut line = vec![
            Span::styled(if selected { ">" } else { " " }, selected_style),
            Span::styled(if marked { "[x] " } else { "[ ] " }, if marked { Style::default().fg(Color::Green) } else { Style::default() }),
            Span::styled(state_marker, marker_style),
            Span::raw(" "),
            Span::raw(bar),
            Span::raw(" "),
            Span::raw(size_text),
            Span::raw(" "),
            Span::raw(link_marker),
            Span::raw(" "),
            Span::raw(entry_name),
            Span::raw(suffix),
        ];
        if selected {
            for span in &mut line {
                span.style = span.style.patch(selected_style);
            }
        }
        frame.render_widget(Paragraph::new(Line::from(line)), row_area);
    }
}

fn row_record<'a>(view: &'a View<'_>, index: usize, parent_offset: usize) -> Option<&'a fdu_core::NodeRecord> {
    let id = view.rows.get(index.checked_sub(parent_offset)?)?;
    view.tree.record(*id)
}

fn size_of(record: &fdu_core::NodeRecord, mode: SizeMode) -> u64 {
    match mode {
        SizeMode::Allocated => record.allocated_bytes,
        SizeMode::Apparent => record.apparent_bytes,
    }
}

fn footer_text(view: &View<'_>) -> Vec<Line<'static>> {
    let hint = match view.phase {
        Phase::Scanning => "Scanning · arrows move · Enter opens · Space marks · r/d unavailable · q cancels",
        Phase::Ready if view.read_only => "Read only · arrows/jk move · Enter opens · h/Backspace returns · a sizes · n/s sort · / filter · q quit",
        Phase::Ready => "arrows/jk move · Enter opens · h/Backspace returns · Space marks · d delete · r rescan · ? help · q quit",
        Phase::Deleting => "Deletion in progress · Esc cancels remaining work · q is unavailable",
    };
    let mut lines = vec![Line::from(hint)];
    if let Some(message) = view.message {
        lines.push(Line::from(message.to_owned()));
    } else if let Some(message) = view.detail_message {
        lines.push(Line::from(format!("Selected entry: {message}")));
    } else if view.range_mode {
        lines.push(Line::from("Range marking is active; sorting and filtering are paused until it ends."));
    } else {
        lines.push(Line::from(format!(
            "{} incomplete · {} excluded · filter {:?}",
            view.incomplete_entries, view.exclusions, view.filter
        )));
    }
    lines
}

fn render_modal(frame: &mut Frame<'_>, view: &View<'_>) {
    let (title, lines) = match &view.modal {
        Modal::None => return,
        Modal::Help => (
            "Help".to_owned(),
            vec![
                "Arrows or j/k move; Enter/right/l opens; left/h/Backspace returns.".to_owned(),
                "Space marks one entry; v starts or ends a stable range; * marks matches.".to_owned(),
                "c clears marks; d deletes marked siblings or the current entry.".to_owned(),
                "n/s sort by name/size; a switches size mode; / filters current names.".to_owned(),
                "r rebuilds the full root index; q quits. Incomplete directories cannot be deleted.".to_owned(),
                "Counted bytes are metadata totals; released storage can differ.".to_owned(),
                "Press Esc or ? to close help.".to_owned(),
            ],
        ),
        Modal::Filter { value } => (
            "Filter current directory".to_owned(),
            vec!["Type a name fragment; Enter applies it, Esc cancels.".to_owned(), (*value).to_owned()],
        ),
        Modal::ConfirmDelete {
            roots,
            descendants,
            apparent_bytes,
            allocated_bytes,
            rejected,
            eligible,
        } => {
            let root_names = roots
                .iter()
                .filter_map(|id| view.tree.name(*id))
                .take(4)
                .map(escape_name)
                .collect::<Vec<_>>();
            let mut lines = if *eligible {
                vec![
                    "Permanently delete these selected entries?".to_owned(),
                    "Enter confirms; Esc cancels. This cannot be undone.".to_owned(),
                ]
            } else {
                vec![
                    "This selection is not eligible for deletion.".to_owned(),
                    "No entries will be deleted. Press Esc to return.".to_owned(),
                ]
            };
            lines.extend(root_names);
            let counts = format!("{} entries in scope · {} allocated · {} apparent", descendants, format_size(*allocated_bytes), format_size(*apparent_bytes));
            lines.push(counts);
            lines.push("Actual storage released may differ from these counts.".to_owned());
            lines.extend(rejected.iter().take(8).cloned());
            ("Confirm permanent deletion".to_owned(), lines)
        }
        Modal::Deleting {
            attempted,
            total,
            current,
            cancellable,
        } => (
            "Deleting".to_owned(),
            vec![
                "Only the confirmed indexed entries are attempted.".to_owned(),
                (*current).to_owned(),
                format!("{} of {} operations completed", attempted, total),
                if *cancellable { "Esc stops scheduling new work." } else { "Waiting for deletion worker shutdown." }.to_owned(),
            ],
        ),
    };
    let available_height = u32::from(frame.area().height.max(1));
    let desired_height = u32::try_from(lines.len()).unwrap_or(u32::MAX).saturating_add(2).max(7);
    let height_percent = desired_height
        .saturating_mul(100)
        .div_ceil(available_height)
        .min(100) as u16;
    let area = centered_rect(frame.area(), 88, height_percent);
    let content = lines.into_iter().map(Line::from).collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().title(title).borders(Borders::ALL))
            .style(Style::default().bg(Color::Black).fg(Color::White)),
        area,
    );
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

pub fn escape_name(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len());
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                append_escaped_text(&mut output, valid);
                break;
            }
            Err(error) => {
                let valid_length = error.valid_up_to();
                if valid_length != 0 {
                    let valid = unsafe { std::str::from_utf8_unchecked(&remaining[..valid_length]) };
                    append_escaped_text(&mut output, valid);
                    remaining = &remaining[valid_length..];
                }
                let invalid_length = error.error_len().unwrap_or(remaining.len()).max(1);
                for byte in &remaining[..invalid_length.min(remaining.len())] {
                    output.push_str(&format!("\\x{byte:02x}"));
                }
                remaining = &remaining[invalid_length.min(remaining.len())..];
            }
        }
    }
    output
}

fn append_escaped_text(output: &mut String, value: &str) {
    for character in value.chars() {
        if character.is_control() {
            output.push_str(&format!("\\u{{{:x}}}", character as u32));
        } else {
            output.push(character);
        }
    }
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut stdout = io::stdout();
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen);
}

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_RESTORE_INSTALLED: OnceLock<()> = OnceLock::new();

fn install_panic_restore() {
    PANIC_RESTORE_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            restore_terminal();
            previous(panic);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::{
        escape_name, map_key, render, Intent, Modal, Phase, SizeMode, SortMode, View,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use fdu_core::{EntryType, FileIdentity, NodeId, NodeState, Tree};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashSet;

    #[test]
    fn display_names_escape_controls_and_preserve_invalid_bytes() {
        assert_eq!(
            escape_name(b"safe\n\x1b-\xff\xc3\xa9"),
            "safe\\u{a}\\u{1b}-\\xffé"
        );
    }

    #[test]
    fn key_mapping_separates_browser_commands_from_filter_text() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(matches!(map_key(key(KeyCode::Char('j'), KeyModifiers::NONE), false), Some(Intent::MoveDown)));
        assert!(matches!(map_key(key(KeyCode::Char(' '), KeyModifiers::NONE), false), Some(Intent::ToggleMark)));
        assert!(matches!(map_key(key(KeyCode::Char('d'), KeyModifiers::NONE), false), Some(Intent::Delete)));
        assert!(matches!(map_key(key(KeyCode::Char('x'), KeyModifiers::NONE), true), Some(Intent::FilterCharacter('x'))));
        assert!(matches!(map_key(key(KeyCode::Esc, KeyModifiers::NONE), true), Some(Intent::Cancel)));
        assert!(matches!(map_key(key(KeyCode::Char('q'), KeyModifiers::NONE), true), Some(Intent::FilterCharacter('q'))));
    }

    #[test]
    fn test_backend_draws_only_the_visible_window_of_a_wide_listing() {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let root = tree.root();
        let mut rows = Vec::new();
        for index in 0..500 {
            let name = format!("entry-{index:04}");
            let id = tree
                .append(
                    root,
                    name.as_bytes(),
                    EntryType::RegularFile,
                    FileIdentity { device: 1, inode: index + 2 },
                    1,
                    index as u64,
                    index as u64,
                    NodeState::Complete,
                )
                .unwrap();
            rows.push(id);
        }
        let marks = HashSet::new();
        let view = View {
            tree: &tree,
            current_directory: root,
            rows: &rows,
            has_parent_row: false,
            cursor_index: Some(rows.len() - 1),
            marks: &marks,
            filter: "",
            size_mode: SizeMode::Allocated,
            sort_mode: SortMode::Size,
            phase: Phase::Ready,
            indexed_entries: rows.len(),
            directory_items: rows.len(),
            visible_items: rows.len(),
            incomplete_entries: 0,
            exclusions: 0,
            read_only: false,
            range_mode: false,
            message: None,
            detail_message: None,
            modal: Modal::None,
        };
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &view)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(output.contains("entry-0499"));
        assert!(!output.contains("entry-0000"));
    }

    #[test]
    fn test_backend_handles_tiny_empty_listing_and_confirmation_modal() {
        let tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let root = tree.root();
        let marks = HashSet::<NodeId>::new();
        let selected = [];
        let rejected = vec!["no entries are selected".to_owned()];
        let view = View {
            tree: &tree,
            current_directory: root,
            rows: &selected,
            has_parent_row: false,
            cursor_index: None,
            marks: &marks,
            filter: "",
            size_mode: SizeMode::Apparent,
            sort_mode: SortMode::Name,
            phase: Phase::Ready,
            indexed_entries: 0,
            directory_items: 0,
            visible_items: 0,
            incomplete_entries: 0,
            exclusions: 0,
            read_only: true,
            range_mode: false,
            message: None,
            detail_message: None,
            modal: Modal::ConfirmDelete {
                roots: &selected,
                descendants: 0,
                apparent_bytes: 0,
                allocated_bytes: 0,
                rejected: &rejected,
                eligible: false,
            },
        };
        let backend = TestBackend::new(12, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &view)).unwrap();
    }
}

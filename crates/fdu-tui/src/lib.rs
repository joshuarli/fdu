mod lscolors;
mod screen;
mod terminal;

use fdu_core::{EntryType, NodeId, NodeRecord, NodeState, Tree};
pub use lscolors::LsColors;
use screen::{Rect, Screen, Style};
use std::collections::HashSet;
use std::io;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub use terminal::{poll_intent, terminal_size};

/// Terminal dimensions in cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

/// How the two panes are arranged. `Vertical` puts them side by side, divided
/// by a vertical line; `Horizontal` stacks them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Split {
    Vertical,
    Horizontal,
}

impl Split {
    pub fn toggled(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

/// Narrowest terminal that shows two panes side by side, and shortest that
/// shows them stacked. Below these the panes would crush each other, so only
/// one pane is drawn at a time.
pub const SPLIT_MIN_COLUMNS: u16 = 60;
pub const STACK_MIN_ROWS: u16 = 12;

pub fn fits_two_panes(split: Split, size: TerminalSize) -> bool {
    match split {
        Split::Vertical => size.columns >= SPLIT_MIN_COLUMNS,
        Split::Horizontal => size.rows >= STACK_MIN_ROWS,
    }
}

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
    ToggleSort,
    StartFilter,
    FilterCharacter(char),
    FilterBackspace,
    SubmitFilter,
    Cancel,
    Help,
    SwitchPane,
    Resize(TerminalSize),
    ToggleSplit,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pane {
    Browser,
    Marked,
}

/// Deletion state shown in the key-hint and status strips. Deletion is
/// confirmed and tracked there rather than in a modal so the marked-items pane
/// stays visible as the review of what will be removed.
pub enum Operation<'a> {
    Idle,
    ConfirmDelete {
        roots: usize,
        entries: usize,
        allocated_bytes: u64,
        apparent_bytes: u64,
    },
    Deleting {
        completed: usize,
        total: usize,
        current: &'a str,
        stopping: bool,
    },
}

pub enum Modal<'a> {
    None,
    Help { focus: Pane },
    Filter { value: &'a str },
}

pub struct View<'a> {
    pub tree: &'a Tree,
    pub current_directory: NodeId,
    pub rows: &'a [NodeId],
    pub has_parent_row: bool,
    pub cursor_index: Option<usize>,
    pub marks: &'a HashSet<NodeId>,
    /// Marked roots in the order the marked-items pane lists them.
    pub marked: &'a [NodeId],
    pub marked_cursor: Option<usize>,
    /// The marked directory that already covers the current directory, if any.
    pub covered_by: Option<NodeId>,
    pub focus: Pane,
    pub side_visible: bool,
    pub split: Split,
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
    pub operation: Operation<'a>,
    pub ls_colors: &'a LsColors,
}

pub struct TerminalSession {
    terminal: terminal::Terminal,
    screen: Screen,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        Ok(Self { terminal: terminal::Terminal::enter()?, screen: Screen::new(0, 0) })
    }

    pub fn draw(&mut self, view: &View<'_>) -> io::Result<()> {
        let size = terminal_size()?;
        let mut next = Screen::new(size.columns, size.rows);
        render(&mut next, view);
        self.terminal.write(&next.diff(&self.screen))?;
        self.screen = next;
        Ok(())
    }

    pub fn restore(&mut self) {
        self.terminal.restore();
    }
}

const BAR_CELLS_WIDE: usize = 10;
const BAR_CELLS_NARROW: usize = 5;
const SIZE_COLUMN_WIDTH: usize = 11;
/// Wide enough that every modal line fits an 80-column terminal.
const MODAL_WIDTH_PERCENT: u16 = 94;

fn render(frame: &mut Screen, view: &View<'_>) {
    let area = frame.area();
    // Short terminals give up the title strip before the panes.
    let title_height = u16::from(area.height >= 8);
    let status_height = u16::from(area.height >= 3);
    let chunks = [
        Rect::new(0, 0, area.width, title_height),
        Rect::new(0, title_height, area.width, area.height - title_height - status_height),
        Rect::new(0, area.height - status_height, area.width, status_height),
    ];
    if title_height > 0 {
        render_title(frame, chunks[0], view);
    }
    render_panes(frame, chunks[1], view);
    if status_height > 0 {
        render_status(frame, chunks[2], view);
    }
    render_modal(frame, view);
}

fn render_title(frame: &mut Screen, area: Rect, view: &View<'_>) {
    // Only state that differs from the defaults is shown.
    let mut text = " fdu".to_owned();
    if view.read_only {
        text.push_str(" · read only");
    }
    if matches!(view.size_mode, SizeMode::Apparent) {
        text.push_str(" · apparent sizes");
    }
    if !view.filter.is_empty() {
        text.push_str(&format!(" · filter {:?}", view.filter));
    }
    let text = pad_to(clip_end(&text, usize::from(area.width)), usize::from(area.width));
    frame.text(area, &text, Style::Reversed);
}

fn render_panes(frame: &mut Screen, area: Rect, view: &View<'_>) {
    let size = TerminalSize { columns: frame.area().width, rows: frame.area().height };
    if !view.side_visible {
        render_browser(frame, area, view);
    } else if fits_two_panes(view.split, size) {
        let parts = match view.split {
            Split::Vertical => area.split_columns(55),
            Split::Horizontal => area.split_rows(60),
        };
        render_browser(frame, parts[0], view);
        render_marked(frame, parts[1], view);
    } else if view.focus == Pane::Marked {
        render_marked(frame, area, view);
    } else {
        render_browser(frame, area, view);
    }
}

/// Focus is shown by normal borders and a bold title; the other pane is dimmed.
fn pane_border(frame: &mut Screen, area: Rect, title: &str, focused: bool) -> Rect {
    frame.border(area, title, if focused { Style::Plain } else { Style::Dim },
        if focused { Style::Bold } else { Style::Dim })
}

fn root_name(view: &View<'_>) -> String {
    escape_name(view.tree.name(view.tree.root()).unwrap_or_default())
}

fn relative_path(view: &View<'_>, node: NodeId) -> String {
    view.tree
        .materialize_path(node)
        .unwrap_or_default()
        .into_iter()
        .map(escape_name)
        .collect::<Vec<_>>()
        .join("/")
}

fn render_browser(frame: &mut Screen, area: Rect, view: &View<'_>) {
    let focused = view.focus == Pane::Browser;
    let current = view.tree.record(view.current_directory);
    let total = current.map_or(0, |record| size_of(record, view.size_mode));
    let relative = relative_path(view, view.current_directory);
    let counts = format!(
        " ({} shown, {} total, {})",
        view.visible_items,
        view.directory_items,
        format_size(total)
    );
    let root = root_name(view);
    let location = if relative.is_empty() { root } else { format!("{root}/{relative}") };
    let inner_width = usize::from(area.width).saturating_sub(2);
    let budget = inner_width.saturating_sub(cell_width(&counts) + 1);
    let title = format!(" {}{counts}", clip_start(&location, budget));
    let inner = pane_border(frame, area, &title, focused);
    render_rows(frame, inner, view, total);
}

fn render_rows(frame: &mut Screen, area: Rect, view: &View<'_>, directory_total: u64) {
    let focused = view.focus == Pane::Browser;
    let parent_offset = usize::from(view.has_parent_row);
    let cursor_index = view.cursor_index;
    let available_rows = usize::from(area.height);
    let row_count = view.rows.len() + parent_offset;
    let start = window_start(cursor_index, row_count, available_rows);
    let visible_end = start.saturating_add(available_rows).min(row_count);
    let width = usize::from(area.width);
    // The name is the most useful column, so the percentage and bar are dropped
    // before the name loses room.
    let show_percent = width >= 46;
    let bar_cells = if width >= 57 {
        BAR_CELLS_WIDE
    } else if width >= 52 {
        BAR_CELLS_NARROW
    } else {
        0
    };

    for screen_row in 0..available_rows {
        let index = start + screen_row;
        if index >= visible_end {
            break;
        }
        let y = area.y.saturating_add(screen_row as u16);
        let row_area = Rect::new(area.x, y, area.width, 1);
        let is_selected = cursor_index == Some(index);
        let selected_style = cursor_style(focused);
        if index < parent_offset {
            let text = pad_to("   ../".to_owned(), width);
            let style = if is_selected { selected_style } else { Style::Dim };
            frame.text(row_area, &text, style);
            continue;
        }
        let Some(id) = view.rows.get(index - parent_offset).copied() else {
            continue;
        };
        let Some(record) = view.tree.record(id) else {
            continue;
        };
        let (marker, size_text) = state_columns(record, view.size_mode);
        let mark = if view.marks.contains(&id) {
            "[x]"
        } else if view.covered_by.is_some() {
            "[=]"
        } else {
            "   "
        };
        let percent = if show_percent {
            percent_column(record, view.size_mode, directory_total)
        } else {
            String::new()
        };
        let bar = bar_column(record, view.size_mode, directory_total, bar_cells);
        let link = if record.link_count > 1 && record.entry_type != EntryType::Directory { "*" } else { " " };
        let suffix = match record.entry_type {
            EntryType::Directory => "/",
            EntryType::Symlink => "@",
            _ => "",
        };
        let prefix = format!(
            "{mark} {marker}{size_text:>size_width$} {percent}{bar}{link}",
            size_width = SIZE_COLUMN_WIDTH,
        );
        let raw_name = view.tree.name(id).unwrap_or_default();
        let name = escape_name(raw_name);
        let name_budget = width.saturating_sub(cell_width(&prefix) + cell_width(suffix));
        let text = pad_to(format!("{prefix}{}{suffix}", clip_end(&name, name_budget)), width);
        let base = row_style(view, id, record, raw_name);
        let style = if is_selected { base.with_cursor(focused) } else { base };
        frame.text(row_area, &text, style);
    }
}

/// A row's color: red when it is marked for deletion, yellow for incomplete or
/// stale, red for excluded, otherwise the `LS_COLORS` file color. The `[x]`,
/// `!`, `~`, `M`, and `A` markers carry the same state in plain text.
fn row_style(view: &View<'_>, id: NodeId, record: &NodeRecord, name: &[u8]) -> Style {
    if view.marks.contains(&id) {
        return Style::Red;
    }
    match record.state {
        NodeState::Incomplete | NodeState::Stale => Style::Yellow,
        NodeState::Excluded(_) => Style::Red,
        _ => view.ls_colors.style_for(name, record.entry_type),
    }
}

fn render_marked(frame: &mut Screen, area: Rect, view: &View<'_>) {
    let focused = view.focus == Pane::Marked;
    let mut total = 0u64;
    for id in view.marked {
        if let Some(record) = view.tree.record(*id) {
            total = total.saturating_add(size_of(record, view.size_mode));
        }
    }
    let root_total = view
        .tree
        .record(view.tree.root())
        .map_or(0, |record| size_of(record, view.size_mode));
    let title = format!(
        " Marked {} item{} ({}, {:.1}% of {})",
        view.marked.len(),
        if view.marked.len() == 1 { "" } else { "s" },
        format_size(total),
        percent_of(total, root_total),
        format_size(root_total),
    );
    let inner_width = usize::from(area.width).saturating_sub(2);
    let inner = pane_border(frame, area, &clip_end(&title, inner_width), focused);

    let width = usize::from(inner.width);
    if view.marked.is_empty() {
        frame.lines(inner, &["Nothing is marked.", "d marks the selected entry."]);
        return;
    }
    let available_rows = usize::from(inner.height);
    let start = window_start(view.marked_cursor, view.marked.len(), available_rows);
    for (screen_row, index) in (start..view.marked.len().min(start + available_rows)).enumerate() {
        let id = view.marked[index];
        let Some(record) = view.tree.record(id) else {
            continue;
        };
        let is_selected = view.marked_cursor == Some(index);
        let (marker, size_text) = state_columns(record, view.size_mode);
        let suffix = match record.entry_type {
            EntryType::Directory => "/",
            EntryType::Symlink => "@",
            _ => "",
        };
        // Size and state come first so a long path is clipped at its start,
        // which keeps the entry's own name and the size readable.
        let prefix = format!(
            " {marker}{size_text:>size_width$}  ",
            size_width = SIZE_COLUMN_WIDTH,
        );
        let path = relative_path(view, id);
        let budget = width.saturating_sub(cell_width(&prefix) + cell_width(suffix));
        let text = pad_to(format!("{prefix}{}{suffix}", clip_start(&path, budget)), width);
        // Every row here is queued for deletion, so every row is red; the
        // cursor keeps its reverse/bold highlight on top of that.
        let style = if is_selected { Style::Red.with_cursor(focused) } else { Style::Red };
        let y = inner.y.saturating_add(screen_row as u16);
        frame.text(Rect::new(inner.x, y, inner.width, 1), &text, style);
    }
}

/// The cursor row is reversed in the focused pane and only bold in the other,
/// so the pane that takes keystrokes is the one with a strong highlight.
fn cursor_style(focused: bool) -> Style {
    if focused { Style::Reversed } else { Style::Bold }
}

/// First row of a window that keeps the cursor visible.
fn window_start(cursor: Option<usize>, row_count: usize, available_rows: usize) -> usize {
    cursor
        .filter(|index| *index < row_count)
        .map(|index| index.saturating_sub(available_rows.saturating_sub(1)))
        .unwrap_or(0)
        .min(row_count.saturating_sub(available_rows))
}

fn state_columns(record: &NodeRecord, mode: SizeMode) -> (&'static str, String) {
    let value = size_of(record, mode);
    let marker = match record.state {
        NodeState::Scanning => "…",
        NodeState::Incomplete => "!",
        NodeState::Excluded(fdu_core::ExclusionReason::MountBoundary) => "M",
        NodeState::Excluded(fdu_core::ExclusionReason::UnsupportedAlias) => "A",
        NodeState::Stale => "~",
        NodeState::Tombstone | NodeState::Complete => " ",
    };
    let size = match record.state {
        NodeState::Excluded(fdu_core::ExclusionReason::MountBoundary) => "mount".to_owned(),
        NodeState::Excluded(fdu_core::ExclusionReason::UnsupportedAlias) => "alias".to_owned(),
        NodeState::Scanning if record.entry_type == EntryType::Directory => "…".to_owned(),
        NodeState::Incomplete | NodeState::Stale => format!("~{}", format_size(value)),
        _ => format_size(value),
    };
    (marker, size)
}

fn has_known_size(record: &NodeRecord) -> bool {
    !matches!(record.state, NodeState::Excluded(_))
        && !(record.state == NodeState::Scanning && record.entry_type == EntryType::Directory)
}

fn percent_of(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 * 100.0 / total as f64
    }
}

fn percent_column(record: &NodeRecord, mode: SizeMode, total: u64) -> String {
    if has_known_size(record) {
        format!("{:>5.1}% ", percent_of(size_of(record, mode), total))
    } else {
        " ".repeat(7)
    }
}

fn bar_column(record: &NodeRecord, mode: SizeMode, total: u64, cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    let value = size_of(record, mode);
    let filled = if !has_known_size(record) || total == 0 || value == 0 {
        0
    } else {
        // Any nonzero entry shows at least one cell so it is not mistaken for empty.
        (((u128::from(value) * cells as u128) + u128::from(total) / 2) / u128::from(total)).clamp(1, cells as u128) as usize
    };
    format!("{}{} ", "█".repeat(filled), " ".repeat(cells - filled))
}

fn size_of(record: &NodeRecord, mode: SizeMode) -> u64 {
    match mode {
        SizeMode::Allocated => record.allocated_bytes,
        SizeMode::Apparent => record.apparent_bytes,
    }
}

/// Key hints for the focused pane, trimmed to what applies.
fn key_hints(view: &View<'_>) -> String {
    match view.focus {
        Pane::Browser if view.marked.is_empty() => "d mark · s sort · / filter · ? help · q quit".to_owned(),
        Pane::Browser => "d mark · Tab marked · - split · s sort · / filter · ? help · q quit".to_owned(),
        Pane::Marked if view.read_only => "d unmark · Enter show · c clear · Tab list · - split · ? help · q quit".to_owned(),
        Pane::Marked => "d unmark · Enter show · ^R delete · c clear · Tab list · - split · ? help · q quit".to_owned(),
    }
}

/// The bottom strip leads with the entry count, which ends in `…` while the
/// scan is still adding to it, followed by a transient message and key hints.
/// A pending confirmation or running deletion has no other place to appear, so
/// it comes first and the count moves to the end.
fn render_status(frame: &mut Screen, area: Rect, view: &View<'_>) {
    let count = format!(
        "{} entries{}",
        view.indexed_entries,
        if view.phase == Phase::Scanning { "…" } else { "" }
    );
    let (text, style) = match &view.operation {
        Operation::Deleting { completed, total, current, stopping } => (
            format!(
                " Deleting {completed} of {total} · {current} · {} · {count}",
                if *stopping { "stopping; removed entries cannot be restored" } else { "Esc stops; removed entries cannot be restored" }
            ),
            Style::Red,
        ),
        Operation::ConfirmDelete { roots, entries, allocated_bytes, .. } => (
            format!(
                " Permanently delete {roots} marked item{} ({entries} entries, {})? Enter confirms · Esc cancels · {count}",
                if *roots == 1 { "" } else { "s" },
                format_size(*allocated_bytes),
            ),
            Style::Red,
        ),
        Operation::Idle => (
            match view.message.or(view.detail_message) {
                Some(message) => format!(" {count} · {message} · {}", key_hints(view)),
                None => format!(" {count} · {}", key_hints(view)),
            },
            Style::Plain,
        ),
    };
    frame.text(area, &clip_end(&text, usize::from(area.width)), style);
}

fn render_modal(frame: &mut Screen, view: &View<'_>) {
    let (title, lines) = match &view.modal {
        Modal::None => return,
        Modal::Help { focus } => help_lines(*focus),
        Modal::Filter { value } => (
            "Filter current directory".to_owned(),
            vec![
                "Type a name fragment; Enter applies it, Esc cancels.".to_owned(),
                format!("/ {value}"),
            ],
        ),
    };
    draw_modal(frame, title, lines);
}

fn help_lines(focus: Pane) -> (String, Vec<String>) {
    let focus = match focus {
        Pane::Browser => "Focus: list",
        Pane::Marked => "Focus: marked items",
    };
    let lines = [
        focus,
        "The marked-items pane opens when something is marked. Tab switches",
        "  panes. - stacks the panes instead of placing them side by side.",
        "  When the terminal is too small for both, only the focused one shows.",
        "",
        "List: j/k or arrows move, Enter/l opens, h/Backspace goes up",
        "  (never above the root). d marks, v marks a range, * marks",
        "  matches, c clears marks, / filters, s sorts by size or name,",
        "  a switches allocated/apparent sizes, r rescans (clears marks).",
        "Marked items: arrows move, d unmarks, Enter shows it in the list,",
        "  Ctrl-R deletes them all after you press Enter to confirm.",
        "",
        "A marked directory covers its subtree: nothing inside it, and no",
        "  directory containing a mark, can be marked until that is removed.",
        "Deletion is permanent; there is no Trash. Byte totals are metadata",
        "  counts and released storage can differ.",
        "Rows: [x] marked (red)  [=] covered  ! incomplete  ~ stale  M mount  A alias",
        "  / directory  @ symlink (never followed)  * hardlinked name",
        "  Directory and symlink colors follow LS_COLORS; marked rows stay red.",
        "The count at the bottom ends in … while the scan is still running.",
        "q or Ctrl-C quits. Esc or ? closes help.",
    ];
    ("Help".to_owned(), lines.iter().map(|line| (*line).to_owned()).collect())
}

fn draw_modal(frame: &mut Screen, title: String, lines: Vec<String>) {
    let available_height = frame.area().height;
    let desired_height = u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(2).max(7);
    let area = modal_area(frame.area(), MODAL_WIDTH_PERCENT, desired_height.min(available_height));
    frame.clear(area);
    let inner = frame.border(area, &title, Style::Plain, Style::Plain);
    frame.lines(inner, &lines);

}

/// A rectangle centered in `area`, `width_percent` wide and `height` rows tall
/// (clamped to the area).
fn modal_area(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = (u32::from(area.width) * u32::from(width_percent) / 100).max(1).min(u32::from(area.width)) as u16;
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// Shortens `text` to at most `max` terminal cells, ending with `…` when cut.
fn clip_end(text: &str, max: usize) -> String {
    if cell_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = char_width(character);
        if used + width > max - 1 {
            break;
        }
        used += width;
        output.push(character);
    }
    output.push('…');
    output
}

/// Shortens `text` to at most `max` terminal cells, keeping its end and
/// starting with `…` when cut. Paths use this so the final name stays visible.
fn clip_start(text: &str, max: usize) -> String {
    if cell_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let width = char_width(character);
        if used + width > max - 1 {
            break;
        }
        used += width;
        kept.push(character);
    }
    // A combining mark whose base character was cut would attach to the ellipsis.
    while kept.last().is_some_and(|character| char_width(*character) == 0) {
        kept.pop();
    }
    let mut output = String::from("…");
    output.extend(kept.into_iter().rev());
    output
}

/// Pads with spaces to exactly `width` cells so a reversed row fills the pane.
fn pad_to(mut text: String, width: usize) -> String {
    let used = cell_width(&text);
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    text
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

#[cfg(test)]
mod tests {
    use super::{
        clip_end, clip_start, escape_name, render, LsColors, Modal, Operation, Pane, Phase,
        SizeMode, SortMode, Split, View,
    };
    use fdu_core::{EntryType, FileIdentity, NodeId, NodeState, Tree};
    use super::screen::{Screen, Style};
    use std::collections::HashSet;

    #[test]
    fn display_names_escape_controls_and_preserve_invalid_bytes() {
        assert_eq!(
            escape_name(b"safe\n\x1b-\xff\xc3\xa9"),
            "safe\\u{a}\\u{1b}-\\xffé"
        );
    }

    #[test]
    fn clipping_counts_terminal_cells_and_keeps_the_requested_end() {
        assert_eq!(clip_end("directory", 6), "direc…");
        assert_eq!(clip_start("a/b/long-name", 7), "…g-name");
        assert_eq!(clip_end("short", 10), "short");
        // Each wide character is two cells, so only two fit before the ellipsis.
        assert_eq!(clip_end("日本語日本語", 6), "日本…");
        assert_eq!(clip_start("日本語日本語", 6), "…本語");
        // A combining mark stays with its base and is not left orphaned after a cut.
        assert_eq!(clip_end("e\u{301}e\u{301}e\u{301}", 2), "e\u{301}…");
        assert_eq!(clip_start("e\u{301}e\u{301}e\u{301}", 2), "…e\u{301}");
        assert_eq!(clip_end("anything", 0), "");
    }

    struct Fixture {
        tree: Tree,
        root: NodeId,
        directory: NodeId,
        nested: NodeId,
        top: NodeId,
    }

    fn fixture() -> Fixture {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let root = tree.root();
        let append = |tree: &mut Tree, parent, name: &[u8], kind, inode, bytes| {
            tree.append(parent, name, kind, FileIdentity { device: 1, inode }, 1, bytes, bytes, NodeState::Complete).unwrap()
        };
        let directory = append(&mut tree, root, b"projects", EntryType::Directory, 2, 0);
        let nested = append(&mut tree, directory, b"payload.bin", EntryType::RegularFile, 3, 4096);
        assert!(tree.add_to_ancestors(directory, 4096, 4096));
        let top = append(&mut tree, root, b"top.log", EntryType::RegularFile, 4, 1024);
        assert!(tree.add_to_ancestors(root, 1024, 1024));
        Fixture { tree, root, directory, nested, top }
    }

    fn draw(view: &View<'_>, columns: u16, rows: u16) -> String {
        buffer(view, columns, rows).plain_text()

    }

    fn base_view<'a>(
        fixture: &'a Fixture,
        rows: &'a [NodeId],
        marks: &'a HashSet<NodeId>,
        marked: &'a [NodeId],
        ls_colors: &'a LsColors,
    ) -> View<'a> {
        View {
            tree: &fixture.tree,
            current_directory: fixture.root,
            rows,
            has_parent_row: false,
            cursor_index: Some(0),
            marks,
            marked,
            marked_cursor: if marked.is_empty() { None } else { Some(0) },
            covered_by: None,
            focus: Pane::Browser,
            side_visible: false,
            split: Split::Vertical,
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
            operation: Operation::Idle,
            ls_colors,
        }
    }

    #[test]
    fn renderer_draws_only_the_visible_window_of_a_wide_listing() {
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
        let fixture = Fixture { tree, root, directory: root, nested: root, top: root };
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &[], &colors);
        view.cursor_index = Some(rows.len() - 1);
        let output = draw(&view, 80, 12);
        assert!(output.contains("entry-0499"));
        assert!(!output.contains("entry-0000"));
    }

    #[test]
    fn two_panes_show_marks_with_root_relative_paths_and_totals() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.nested, fixture.top]);
        let marked = [fixture.nested, fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        view.focus = Pane::Marked;
        let output = draw(&view, 120, 24);
        assert!(output.contains("Marked 2 items (5.0 KiB, 100.0% of 5.0 KiB)"), "{output}");
        assert!(output.contains("projects/payload.bin"), "{output}");
        assert!(output.contains("top.log"), "{output}");
    }

    fn buffer(view: &View<'_>, columns: u16, rows: u16) -> Screen {
        let mut screen = Screen::new(columns, rows);
        render(&mut screen, view);
        screen
    }

    #[test]
    fn the_unfocused_pane_is_dimmed_and_the_focused_cursor_row_is_reversed() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let marked = [fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        // Column 0 is the list's left border, the last column the marked pane's.
        let list_border = |buffer: &Screen| buffer[(0, 2)].style;
        let marked_border = |buffer: &Screen| buffer[(109, 2)].style;
        let list_focused = buffer(&view, 110, 20);
        assert!(list_border(&list_focused) != Style::Dim);
        assert!(marked_border(&list_focused) == Style::Dim);
        assert!(list_focused[(5, 2)].style.reversed);
        assert!(!list_focused[(5, 2)].style.dim);

        view.focus = Pane::Marked;
        let marked_focused = buffer(&view, 110, 20);
        assert!(list_border(&marked_focused) == Style::Dim);
        assert!(marked_border(&marked_focused) != Style::Dim);
        assert!(!marked_focused[(5, 2)].style.reversed);
        assert!(marked_focused[(70, 2)].style.reversed);
    }

    #[test]
    fn horizontal_split_stacks_the_panes_when_there_is_height_for_both() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let marked = [fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        view.split = Split::Horizontal;
        let output = draw(&view, 60, 20);
        let lines: Vec<&str> = output.lines().collect();
        let list_row = lines.iter().position(|line| line.contains("root (")).unwrap();
        let marked_row = lines.iter().position(|line| line.contains("Marked 1 item")).unwrap();
        assert!(list_row < marked_row, "{output}");
        assert!(lines[list_row].trim_end().ends_with('┐'), "the list spans the full width: {output}");

        // Too short for two stacked panes: only the list shows.
        let output = draw(&view, 60, 8);
        assert!(!output.contains("Marked 1 item"), "{output}");
    }

    #[test]
    fn narrow_terminal_shows_only_the_marked_pane_when_the_side_is_open() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let marked = [fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        view.focus = Pane::Marked;
        let narrow = draw(&view, 50, 24);
        assert!(narrow.contains("Marked 1 item"), "{narrow}");
        assert!(!narrow.contains("projects"), "{narrow}");
        view.side_visible = false;
        view.focus = Pane::Browser;
        let collapsed = draw(&view, 50, 24);
        assert!(collapsed.contains("projects"), "{collapsed}");
        assert!(!collapsed.contains("Marked 1 item"), "{collapsed}");
    }

    #[test]
    fn rows_cue_marked_and_covered_entries() {
        let fixture = fixture();
        let rows = [fixture.nested];
        let marks = HashSet::new();
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &[], &colors);
        view.current_directory = fixture.directory;
        view.covered_by = Some(fixture.directory);
        assert!(draw(&view, 80, 12).contains("[=]"));
        let marks = HashSet::from([fixture.nested]);
        let mut view = base_view(&fixture, &rows, &marks, &[], &colors);
        view.current_directory = fixture.directory;
        assert!(draw(&view, 80, 12).contains("[x]"));
        // Marked rows are red so the deletion basket stands out (with reverse
        // on top while the cursor is on them).
        let marked_row = buffer(&view, 80, 12);
        assert!(marked_row[(2, 2)].style.reversed);
        assert!(marked_row[(2, 2)].style.fg.is_some_and(|color| matches!(color, super::screen::Color::Red)));
    }

    #[test]
    fn directories_follow_ls_colors_and_marked_rows_stay_red() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &[], &colors);
        // Cursor starts on the directory; move it to the marked file so the
        // directory row shows its base color without the cursor highlight.
        view.cursor_index = Some(1);
        let screen = buffer(&view, 80, 12);
        let directory_style = screen[(2, 2)].style;
        assert!(directory_style.fg.is_some(), "directories use LS_COLORS: {directory_style:?}");
        assert!(!directory_style.reversed);
        // The marked file row stays red with the cursor's reverse on top.
        let marked_style = screen[(2, 3)].style;
        assert!(marked_style.fg.is_some_and(|color| matches!(color, super::screen::Color::Red)));
        assert!(marked_style.reversed, "the cursor keeps reverse on top of red: {marked_style:?}");
    }

    #[test]
    fn marked_pane_and_delete_status_are_red() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let marked = [fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        view.focus = Pane::Marked;
        // Marked pane rows are red; the selected one keeps reverse on top.
        let screen = buffer(&view, 110, 20);
        assert!(screen[(70, 2)].style.reversed);
        assert!(screen[(70, 2)].style.fg.is_some());
        // Confirmation and progress strips warn in red.
        view.operation = Operation::ConfirmDelete { roots: 1, entries: 1, allocated_bytes: 1, apparent_bytes: 1 };
        let confirming = buffer(&view, 100, 24);
        assert!(confirming[(1, 23)].style == Style::Red);
        view.operation = Operation::Deleting { completed: 0, total: 1, current: "x", stopping: false };
        let deleting = buffer(&view, 100, 24);
        assert!(deleting[(1, 23)].style == Style::Red);
    }

    #[test]
    fn confirmation_and_progress_live_in_the_strips_and_keep_the_marked_pane_visible() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.nested, fixture.top]);
        let marked = [fixture.nested, fixture.top];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
        view.side_visible = true;
        view.focus = Pane::Marked;
        view.operation = Operation::ConfirmDelete { roots: 2, entries: 2, allocated_bytes: 5120, apparent_bytes: 5000 };
        let output = draw(&view, 100, 24);
        assert!(output.contains("Permanently delete 2 marked items (2 entries, 5.0 KiB)? Enter confirms"), "{output}");
        assert!(output.contains("projects/payload.bin"), "the marked pane is still the review: {output}");
        assert!(!output.contains("┌Confirm"), "no modal: {output}");

        view.phase = Phase::Deleting;
        view.operation = Operation::Deleting { completed: 1, total: 3, current: "top.log", stopping: false };
        let output = draw(&view, 100, 24);
        assert!(output.contains("Deleting 1 of 3 · top.log"), "{output}");
        assert!(output.contains("removed entries cannot be restored"), "{output}");
    }

    #[test]
    fn status_strip_leads_with_the_entry_count_then_key_hints() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::new();
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &rows, &marks, &[], &colors);
        view.indexed_entries = 42;
        let ready = draw(&view, 100, 24);
        let status = ready.lines().last().unwrap().trim().to_owned();
        assert!(status.starts_with("42 entries · d mark"), "{status}");
        assert!(status.contains("? help"), "{status}");
        view.phase = Phase::Scanning;
        let scanning = draw(&view, 100, 24);
        assert!(scanning.lines().last().unwrap().trim().starts_with("42 entries… · "));
        let title = scanning.lines().next().unwrap();
        for noise in ["Ready", "allocated", "sort", "Scanning"] {
            assert!(!title.contains(noise), "{noise} is not title noise: {title}");
        }
    }

    #[test]
    fn every_terminal_size_and_modal_draws_without_panicking() {
        let fixture = fixture();
        let rows = [fixture.directory, fixture.top];
        let marks = HashSet::from([fixture.top]);
        let marked = [fixture.top];
        let colors = LsColors::default();
        for columns in [1, 2, 5, 12, 36, 59, 60, 100, 140] {
            for height in [1, 2, 3, 5, 8, 12, 30] {
                for modal in 0..6 {
                    let mut view = base_view(&fixture, &rows, &marks, &marked, &colors);
                    view.side_visible = true;
                    view.focus = Pane::Marked;
                    view.has_parent_row = true;
                    match modal {
                        0 => {}
                        1 => view.modal = Modal::Help { focus: Pane::Browser },
                        2 => view.modal = Modal::Filter { value: "abc" },
                        3 => view.operation = Operation::ConfirmDelete { roots: 1, entries: 1, allocated_bytes: 1, apparent_bytes: 1 },
                        4 => view.operation = Operation::Deleting { completed: 0, total: 1, current: "x", stopping: true },
                        _ => view.phase = Phase::Scanning,
                    }
                    draw(&view, columns, height);
                    view.side_visible = false;
                    draw(&view, columns, height);
                }
            }
        }
    }

    #[test]
    fn renderer_handles_tiny_empty_listing_and_confirmation_modal() {
        let tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let root = tree.root();
        let fixture = Fixture { tree, root, directory: root, nested: root, top: root };
        let marks = HashSet::<NodeId>::new();
        let selected = [];
        let colors = LsColors::default();
        let mut view = base_view(&fixture, &selected, &marks, &[], &colors);
        view.size_mode = SizeMode::Apparent;
        view.read_only = true;
        view.modal = Modal::Help { focus: Pane::Marked };
        draw(&view, 12, 5);
    }

    #[test]
    fn names_with_wide_combining_and_control_characters_stay_within_the_pane() {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let root = tree.root();
        let mut rows = Vec::new();
        for (index, name) in [&b"\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e-wide-name-that-is-long"[..], b"e\xcc\x81e\xcc\x81-combining", b"tab\tname\xff"].into_iter().enumerate() {
            rows.push(
                tree.append(root, name, EntryType::RegularFile, FileIdentity { device: 1, inode: index as u64 + 2 }, 1, 10, 10, NodeState::Complete).unwrap(),
            );
        }
        let marks = HashSet::new();
        let fixture = Fixture { tree, root, directory: root, nested: root, top: root };
        let colors = LsColors::default();
        let view = base_view(&fixture, &rows, &marks, &[], &colors);
        let output = draw(&view, 40, 10);
        assert!(output.contains("tab\\u{9}name\\xff"), "{output}");
        for line in output.lines().skip(2).take(3) {
            assert!(line.ends_with('│'), "row overflowed its pane border: {line:?}");
        }
    }
}

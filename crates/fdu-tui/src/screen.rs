use std::fmt::Write;
use std::ops::Index;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self { x, y, width, height }
    }

    pub fn split_columns(self, percent: u16) -> [Self; 2] {
        let width = ((u32::from(self.width) * u32::from(percent) + 50) / 100) as u16;
        [Self { width, ..self }, Self::new(self.x + width, self.y, self.width - width, self.height)]
    }

    pub fn split_rows(self, percent: u16) -> [Self; 2] {
        let height = ((u32::from(self.height) * u32::from(percent) + 50) / 100) as u16;
        [Self { height, ..self }, Self::new(self.x, self.y + height, self.width, self.height - height)]
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq, Debug)]
pub(crate) struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub reversed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Ansi256(u8),
    Rgb(u8, u8, u8),
}

impl Style {
    #[allow(non_upper_case_globals)]
    pub const Plain: Self = Self { fg: None, bg: None, bold: false, dim: false, reversed: false };
    #[allow(non_upper_case_globals)]
    pub const Bold: Self = Self { fg: None, bg: None, bold: true, dim: false, reversed: false };
    #[allow(non_upper_case_globals)]
    pub const Dim: Self = Self { fg: None, bg: None, bold: false, dim: true, reversed: false };
    #[allow(non_upper_case_globals)]
    pub const Reversed: Self = Self { fg: None, bg: None, bold: false, dim: false, reversed: true };
    #[allow(non_upper_case_globals)]
    pub const Yellow: Self = Self {
        fg: Some(Color::Yellow),
        bg: None,
        bold: false,
        dim: false,
        reversed: false,
    };
    #[allow(non_upper_case_globals)]
    pub const Red: Self = Self {
        fg: Some(Color::Red),
        bg: None,
        bold: false,
        dim: false,
        reversed: false,
    };

    /// Keeps the foreground/background while adding the cursor highlight, so a
    /// selected row stays reversed (or bold when unfocused) without losing its color.
    pub fn with_cursor(self, focused: bool) -> Self {
        if focused {
            Self { reversed: true, ..self }
        } else {
            Self { bold: true, ..self }
        }
    }

    fn ansi(self) -> String {
        let mut output = String::from("\x1b[0");
        if self.bold {
            output.push_str(";1");
        }
        if self.dim {
            output.push_str(";2");
        }
        if self.reversed {
            output.push_str(";7");
        }
        if let Some(color) = self.fg {
            match color {
                Color::Black => output.push_str(";30"),
                Color::Red => output.push_str(";31"),
                Color::Green => output.push_str(";32"),
                Color::Yellow => output.push_str(";33"),
                Color::Blue => output.push_str(";34"),
                Color::Magenta => output.push_str(";35"),
                Color::Cyan => output.push_str(";36"),
                Color::White => output.push_str(";37"),
                Color::BrightBlack => output.push_str(";90"),
                Color::BrightRed => output.push_str(";91"),
                Color::BrightGreen => output.push_str(";92"),
                Color::BrightYellow => output.push_str(";93"),
                Color::BrightBlue => output.push_str(";94"),
                Color::BrightMagenta => output.push_str(";95"),
                Color::BrightCyan => output.push_str(";96"),
                Color::BrightWhite => output.push_str(";97"),
                Color::Ansi256(index) => {
                    output.push_str(&format!(";38;5;{index}"));
                }
                Color::Rgb(red, green, blue) => {
                    output.push_str(&format!(";38;2;{red};{green};{blue}"));
                }
            }
        }
        if let Some(color) = self.bg {
            match color {
                Color::Black => output.push_str(";40"),
                Color::Red => output.push_str(";41"),
                Color::Green => output.push_str(";42"),
                Color::Yellow => output.push_str(";43"),
                Color::Blue => output.push_str(";44"),
                Color::Magenta => output.push_str(";45"),
                Color::Cyan => output.push_str(";46"),
                Color::White => output.push_str(";47"),
                Color::BrightBlack => output.push_str(";100"),
                Color::BrightRed => output.push_str(";101"),
                Color::BrightGreen => output.push_str(";102"),
                Color::BrightYellow => output.push_str(";103"),
                Color::BrightBlue => output.push_str(";104"),
                Color::BrightMagenta => output.push_str(";105"),
                Color::BrightCyan => output.push_str(";106"),
                Color::BrightWhite => output.push_str(";107"),
                Color::Ansi256(index) => {
                    output.push_str(&format!(";48;5;{index}"));
                }
                Color::Rgb(red, green, blue) => {
                    output.push_str(&format!(";48;2;{red};{green};{blue}"));
                }
            }
        }
        output.push('m');
        output
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub(crate) struct Cell {
    text: String,
    pub style: Style,
}

/// Empty cell text is reserved for the trailing cell of a wide glyph.
/// Keeping a complete screen lets redraws erase old content and emit only changes.
pub(crate) struct Screen {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        Self { width, height, cells: vec![Cell { text: " ".into(), style: Style::Plain }; usize::from(width) * usize::from(height)] }
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn cell_mut(&mut self, x: u16, y: u16) -> &mut Cell {
        &mut self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)]
    }

    pub fn text(&mut self, area: Rect, text: &str, style: Style) {
        if area.height == 0 || area.y >= self.height {
            return;
        }
        let end = area.x.saturating_add(area.width).min(self.width);
        let mut x = area.x;
        let mut glyph = String::new();
        // Width is measured on each complete glyph so combining marks and joined
        // emoji occupy the same cells as their base rather than shifting the border.
        for character in text.chars() {
            if glyph.is_empty() {
                glyph.push(character);
                continue;
            }
            let width = UnicodeWidthStr::width(glyph.as_str());
            glyph.push(character);
            if UnicodeWidthChar::width(character) == Some(0)
                || UnicodeWidthStr::width(glyph.as_str()) <= width {
                continue;
            }
            glyph.pop();
            if !self.glyph(&mut x, end, area.y, &glyph, style) {
                return;
            }
            glyph.clear();
            glyph.push(character);
        }
        if !glyph.is_empty() {
            self.glyph(&mut x, end, area.y, &glyph, style);
        }
    }

    fn glyph(&mut self, x: &mut u16, end: u16, y: u16, text: &str, style: Style) -> bool {
        let width = UnicodeWidthStr::width(text);
        if width == 0 {
            return true;
        }
        if usize::from(*x) + width > usize::from(end) {
            return false;
        }
        for offset in 0..width {
            self.erase_wide_glyph(*x + offset as u16, y);
        }
        *self.cell_mut(*x, y) = Cell { text: text.to_owned(), style };
        for offset in 1..width {
            *self.cell_mut(*x + offset as u16, y) = Cell { text: String::new(), style };
        }
        *x += width as u16;
        true
    }

    // A terminal erases an entire wide glyph when either cell is overwritten.
    // Mirror that in the screen so later diffs can restore its leading cell.
    fn erase_wide_glyph(&mut self, x: u16, y: u16) {
        let mut lead = x;
        while lead > 0 && self[(lead, y)].text.is_empty() {
            lead -= 1;
        }
        let width = UnicodeWidthStr::width(self[(lead, y)].text.as_str()) as u16;
        if width > 1 {
            for column in lead..(lead + width).min(self.width) {
                *self.cell_mut(column, y) = Cell { text: " ".into(), style: Style::Plain };
            }
        }
    }

    pub fn lines(&mut self, area: Rect, lines: &[impl AsRef<str>]) {
        for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
            self.text(Rect::new(area.x, area.y + row as u16, area.width, 1), line.as_ref(), Style::Plain);
        }
    }

    pub fn clear(&mut self, area: Rect) {
        for y in area.y..area.y.saturating_add(area.height).min(self.height) {
            for x in area.x..area.x.saturating_add(area.width).min(self.width) {
                self.erase_wide_glyph(x, y);
                *self.cell_mut(x, y) = Cell { text: " ".into(), style: Style::Plain };
            }
        }
    }

    pub fn border(&mut self, area: Rect, title: &str, border: Style, title_style: Style) -> Rect {
        if area.width == 0 || area.height == 0 {
            return Rect::new(area.x, area.y, 0, 0);
        }
        let right = area.x + area.width - 1;
        let bottom = area.y + area.height - 1;
        for y in area.y..=bottom {
            self.text(Rect::new(area.x, y, 1, 1), "│", border);
            self.text(Rect::new(right, y, 1, 1), "│", border);
        }
        let horizontal = "─".repeat(usize::from(area.width));
        self.text(Rect::new(area.x, area.y, area.width, 1), &horizontal, border);
        self.text(Rect::new(area.x, bottom, area.width, 1), &horizontal, border);
        for (x, y, corner) in [(area.x, area.y, "┌"), (right, area.y, "┐"), (area.x, bottom, "└"), (right, bottom, "┘")] {
            self.text(Rect::new(x, y, 1, 1), corner, border);
        }
        self.text(Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1), title, title_style);
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2))
    }

    pub fn diff(&self, previous: &Self) -> Vec<u8> {
        let resized = self.width != previous.width || self.height != previous.height;
        let mut output = String::new();
        if resized {
            output.push_str("\x1b[0m\x1b[2J");
        }
        let mut cursor = None;
        let mut style = Style::Plain;
        for y in 0..self.height {
            for x in 0..self.width {
                let cell = &self[(x, y)];
                if cell.text.is_empty() || (resized && cell.text == " " && cell.style == Style::Plain)
                    || (!resized && cell == &previous[(x, y)]) {
                    continue;
                }
                if cursor != Some((x, y)) {
                    write!(output, "\x1b[{};{}H", y + 1, x + 1).unwrap();
                }
                if style != cell.style {
                    output.push_str(&cell.style.ansi());
                    style = cell.style;
                }
                output.push_str(&cell.text);
                cursor = Some((x + UnicodeWidthStr::width(cell.text.as_str()) as u16, y));
            }
        }
        if style != Style::Plain {
            output.push_str(&Style::Plain.ansi());
        }
        output.into_bytes()
    }

    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.cells.chunks(usize::from(self.width)).map(|row| {
            row.iter().map(|cell| cell.text.as_str()).collect::<String>()
        }).collect::<Vec<_>>().join("\n")
    }
}

impl Index<(u16, u16)> for Screen {
    type Output = Cell;

    fn index(&self, (x, y): (u16, u16)) -> &Cell {
        &self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)]
    }
}

#[cfg(test)]
mod tests {
    use super::{Rect, Screen, Style};

    #[test]
    fn closing_an_overlay_repaints_a_wide_glyph_it_partially_covered() {
        let mut listing = Screen::new(4, 1);
        listing.text(listing.area(), "日ab", Style::Plain);
        let mut overlay = Screen::new(4, 1);
        overlay.text(overlay.area(), "日ab", Style::Plain);
        overlay.clear(Rect::new(1, 0, 1, 1));
        overlay.text(Rect::new(1, 0, 1, 1), "|", Style::Plain);
        let restored = String::from_utf8(listing.diff(&overlay)).unwrap();
        assert!(restored.contains('日'), "closing the overlay must redraw the complete glyph: {restored:?}");
    }
}

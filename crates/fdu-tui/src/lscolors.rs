use crate::screen::{Color, Style};
use fdu_core::EntryType;

/// File colors parsed from the standard `LS_COLORS` environment variable.
///
/// Only the parts the index can answer are used: `di` for directories, `ln`
/// for symlinks, `fi` for regular files, and `*` patterns for name matches.
/// Permission-based keys (`ex`, `su`, `sg`, `tw`, `ow`, `st`, `ca`) are parsed
/// but ignored because the index retains entry types and names, not mode bits.
#[derive(Clone, Debug)]
pub struct LsColors {
    dir: Style,
    file: Style,
    symlink: Style,
    extensions: Vec<(String, Style)>,
}

impl Default for LsColors {
    fn default() -> Self {
        Self {
            dir: Style { fg: Some(Color::Blue), bold: true, ..Style::Plain },
            file: Style::Plain,
            symlink: Style { fg: Some(Color::Cyan), bold: true, ..Style::Plain },
            extensions: Vec::new(),
        }
    }
}

impl LsColors {
    /// Reads `LS_COLORS` once at startup. Missing, non-UTF-8, or empty values
    /// fall back to the built-in directory/symlink defaults.
    pub fn from_env() -> Self {
        match std::env::var("LS_COLORS") {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            _ => Self::default(),
        }
    }

    fn parse(value: &str) -> Self {
        let mut colors = Self::default();
        for entry in value.split(':') {
            let Some((key, codes)) = entry.split_once('=') else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            if key.starts_with('*') {
                if let Some(style) = parse_sgr(codes) {
                    colors.extensions.push((key.to_owned(), style));
                }
                continue;
            }
            let style = match parse_sgr(codes) {
                Some(style) => style,
                None => continue,
            };
            match key {
                "di" => colors.dir = style,
                "fi" => colors.file = style,
                "ln" => colors.symlink = style,
                _ => {}
            }
        }
        colors
    }

    /// The color for one row. Directories and symlinks use their type; other
    /// entries try `*` patterns first (in `LS_COLORS` order) and fall back to `fi`.
    pub(crate) fn style_for(&self, name: &[u8], entry_type: EntryType) -> Style {
        match entry_type {
            EntryType::Directory | EntryType::Root => self.dir,
            EntryType::Symlink => self.symlink,
            EntryType::RegularFile | EntryType::Other => {
                for (pattern, style) in &self.extensions {
                    if glob_match(pattern.as_bytes(), name) {
                        return *style;
                    }
                }
                self.file
            }
        }
    }
}

/// Parses an `LS_COLORS` SGR value such as `01;34` into a [`Style`].
/// Returns `None` for the special `target` value and for values with no
/// usable color, so callers keep their previous color.
fn parse_sgr(value: &str) -> Option<Style> {
    if value.is_empty() || value == "target" {
        return None;
    }
    let mut style = Style::Plain;
    let mut seen = false;
    let parts: Vec<&str> = value.split(';').collect();
    let mut index = 0;
    while index < parts.len() {
        let code = parts[index].trim();
        index += 1;
        // Empty fields (from `;;` or a trailing `;`) carry no color.
        let number: u32 = match code.parse() {
            Ok(number) => number,
            Err(_) => continue,
        };
        seen = true;
        match number {
            0 => style = Style::Plain,
            1 => style.bold = true,
            2 => style.dim = true,
            7 => style.reversed = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            27 => style.reversed = false,
            30..=37 => style.fg = Some(basic(number - 30)),
            90..=97 => style.fg = Some(bright(number - 90)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(basic(number - 40)),
            100..=107 => style.bg = Some(bright(number - 100)),
            49 => style.bg = None,
            38 | 48 => {
                let is_fg = number == 38;
                let Some(mode) = parts.get(index).and_then(|part| part.trim().parse::<u32>().ok()) else {
                    continue;
                };
                index += 1;
                match mode {
                    5 => {
                        let Some(n) = parts.get(index).and_then(|part| part.trim().parse::<u8>().ok()) else {
                            continue;
                        };
                        index += 1;
                        if is_fg {
                            style.fg = Some(Color::Ansi256(n));
                        } else {
                            style.bg = Some(Color::Ansi256(n));
                        }
                    }
                    2 => {
                        let channels: Option<[u8; 3]> = (0..3)
                            .map(|offset| {
                                parts.get(index + offset).and_then(|part| part.trim().parse::<u8>().ok())
                            })
                            .collect::<Option<Vec<_>>>()
                            .and_then(|channels| channels.try_into().ok());
                        let Some([red, green, blue]) = channels else {
                            continue;
                        };
                        index += 3;
                        if is_fg {
                            style.fg = Some(Color::Rgb(red, green, blue));
                        } else {
                            style.bg = Some(Color::Rgb(red, green, blue));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if seen { Some(style) } else { None }
}

fn basic(index: u32) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::White,
    }
}

fn bright(index: u32) -> Color {
    match index {
        0 => Color::BrightBlack,
        1 => Color::BrightRed,
        2 => Color::BrightGreen,
        3 => Color::BrightYellow,
        4 => Color::BrightBlue,
        5 => Color::BrightMagenta,
        6 => Color::BrightCyan,
        _ => Color::BrightWhite,
    }
}

/// Matches an `LS_COLORS` `*` pattern against a filename. `*` spans any
/// number of bytes; matching is byte-wise and case-sensitive like GNU ls.
fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut px, mut nx) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while nx < name.len() {
        if px < pattern.len() && pattern[px] == b'*' {
            star = Some(px);
            mark = nx;
            px += 1;
        } else if px < pattern.len() && pattern[px] == name[nx] {
            px += 1;
            nx += 1;
        } else if let Some(star_pos) = star {
            mark += 1;
            nx = mark;
            px = star_pos + 1;
        } else {
            return false;
        }
    }
    while px < pattern.len() && pattern[px] == b'*' {
        px += 1;
    }
    px == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_color_directories_and_symlinks() {
        let colors = LsColors::default();
        assert_eq!(
            colors.style_for(b"projects", EntryType::Directory),
            Style { fg: Some(Color::Blue), bold: true, ..Style::Plain }
        );
        assert_eq!(
            colors.style_for(b"link", EntryType::Symlink),
            Style { fg: Some(Color::Cyan), bold: true, ..Style::Plain }
        );
        assert_eq!(colors.style_for(b"file", EntryType::RegularFile), Style::Plain);
    }

    #[test]
    fn parses_type_colors_and_extension_patterns_in_order() {
        let colors = LsColors::parse("di=01;34:ln=01;36:fi=0:*.tar=01;31:*.gz=01;35:");
        assert_eq!(
            colors.style_for(b"dir", EntryType::Directory),
            Style { fg: Some(Color::Blue), bold: true, ..Style::Plain }
        );
        assert_eq!(colors.style_for(b"plain", EntryType::RegularFile), Style::Plain);
        assert_eq!(
            colors.style_for(b"archive.tar", EntryType::RegularFile),
            Style { fg: Some(Color::Red), bold: true, ..Style::Plain }
        );
        assert_eq!(
            colors.style_for(b"archive.tgz", EntryType::RegularFile),
            Style::Plain,
            "unlisted suffixes keep the file color"
        );
    }

    #[test]
    fn type_colors_win_over_extensions() {
        let colors = LsColors::parse("di=01;34:*.tar=01;31:");
        assert_eq!(
            colors.style_for(b"archive.tar", EntryType::Directory),
            Style { fg: Some(Color::Blue), bold: true, ..Style::Plain },
            "a directory named *.tar still uses di"
        );
    }

    #[test]
    fn parses_extended_and_bright_colors() {
        let colors = LsColors::parse("di=38;5;27:ln=38;2;0;255;255:fi=90:");
        assert_eq!(
            colors.style_for(b"d", EntryType::Directory),
            Style { fg: Some(Color::Ansi256(27)), ..Style::Plain }
        );
        assert_eq!(
            colors.style_for(b"l", EntryType::Symlink),
            Style { fg: Some(Color::Rgb(0, 255, 255)), ..Style::Plain }
        );
        assert_eq!(
            colors.style_for(b"f", EntryType::RegularFile),
            Style { fg: Some(Color::BrightBlack), ..Style::Plain }
        );
    }

    #[test]
    fn ignores_malformed_entries_and_keeps_defaults() {
        let colors = LsColors::parse("di=:no-equals:ln=target:fi=01;not-a-number:");
        assert_eq!(
            colors.style_for(b"d", EntryType::Directory),
            Style { fg: Some(Color::Blue), bold: true, ..Style::Plain },
            "empty di keeps the default"
        );
        assert_eq!(
            colors.style_for(b"l", EntryType::Symlink),
            Style { fg: Some(Color::Cyan), bold: true, ..Style::Plain },
            "ln=target keeps the default"
        );
    }

    #[test]
    fn glob_star_spans_bytes_case_sensitively() {
        assert!(glob_match(b"*.tar.gz", b"archive.tar.gz"));
        assert!(!glob_match(b"*.tar.gz", b"archive.TAR.GZ"));
        assert!(glob_match(b"*README", b"README"));
        assert!(glob_match(b"*README", b"docs-README"));
        assert!(!glob_match(b"*.tar", b"archive.tar.gz"));
        assert!(!glob_match(b"*.tar", b"tar"));
    }
}

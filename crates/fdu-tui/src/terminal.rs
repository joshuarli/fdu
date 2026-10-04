use crate::{Intent, TerminalSize};
use rustix::event::{self, FdSetElement, Timespec};
use rustix::termios::{self, OptionalActions, Termios};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(30);
const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H";
const LEAVE_SCREEN: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1049l";

static ORIGINAL_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static INPUT: Mutex<Input> = Mutex::new(Input {
    decoder: Decoder { bytes: Vec::new(), escape_since: None },
    size: None,
});
static PANIC_RESTORE_INSTALLED: OnceLock<()> = OnceLock::new();

pub(crate) struct Terminal {
    active: bool,
}

impl Terminal {
    pub(crate) fn enter() -> io::Result<Self> {
        install_panic_restore();
        let size = terminal_size()?;
        let stdin = io::stdin();
        let mut original = ORIGINAL_TERMIOS.lock().map_err(|_| io::Error::other("terminal state poisoned"))?;
        if original.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "terminal session already active"));
        }
        let saved = termios::tcgetattr(&stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        *INPUT.lock().map_err(|_| io::Error::other("terminal input poisoned"))? = Input {
            decoder: Decoder::default(),
            size: Some(size),
        };
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        *original = Some(saved);
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        drop(original);
        let mut terminal = Self { active: true };
        if let Err(error) = terminal.write(ENTER_SCREEN) {
            terminal.restore();
            return Err(error);
        }
        Ok(terminal)
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !self.active {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "terminal session restored"));
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()
    }

    pub(crate) fn restore(&mut self) {
        if self.active {
            self.active = false;
            restore_terminal();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn terminal_size() -> io::Result<TerminalSize> {
    let size = termios::tcgetwinsize(io::stdout())?;
    Ok(TerminalSize { columns: size.ws_col, rows: size.ws_row })
}

struct Input {
    decoder: Decoder,
    size: Option<TerminalSize>,
}

pub fn poll_intent(timeout: Duration, text_input: bool) -> io::Result<Option<Intent>> {
    if !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "terminal session inactive"));
    }
    let mut input = INPUT.lock().map_err(|_| io::Error::other("terminal input poisoned"))?;
    let started = Instant::now();
    let stdin = io::stdin();
    loop {
        let size = terminal_size()?;
        if input.size != Some(size) {
            input.size = Some(size);
            return Ok(Some(Intent::Resize(size)));
        }
        let now = Instant::now();
        match input.decoder.next(now)? {
            Decoded::Key(key) => return Ok(map_key(key, text_input)),
            Decoded::Ignored => continue,
            Decoded::Pending => {}
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let wait = input.decoder.escape_wait(now).map_or(remaining, |escape| remaining.min(escape));
        match input_ready(&stdin, wait) {
            Ok(false) => {
                if started.elapsed() >= timeout {
                    return match input.decoder.next(Instant::now())? {
                        Decoded::Key(key) => Ok(map_key(key, text_input)),
                        _ => Ok(None),
                    };
                }
                continue;
            }
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if started.elapsed() >= timeout {
                    return Ok(None);
                }
                continue;
            }
            Err(error) => return Err(error),
        }
        let mut bytes = [0; 256];
        match rustix::io::read(&stdin, &mut bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
            Ok(length) => input.decoder.feed(&bytes[..length]),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

// select supports terminal descriptors on macOS, where poll can reject them.
fn input_ready(stdin: &io::Stdin, timeout: Duration) -> io::Result<bool> {
    let descriptor = stdin.as_raw_fd();
    let mut descriptors = vec![FdSetElement::default(); event::fd_set_num_elements(1, descriptor + 1)];
    event::fd_set_insert(&mut descriptors, descriptor);
    let timeout = Timespec::try_from(timeout).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "terminal poll timeout too large"))?;
    // The only descriptor in the set is borrowed from the live stdin handle.
    let ready = unsafe { event::select(descriptor + 1, Some(&mut descriptors), None, None, Some(&timeout)) }?;
    Ok(ready != 0)
}

fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let saved = ORIGINAL_TERMIOS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    if let Some(saved) = saved {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &saved);
    }
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(LEAVE_SCREEN);
    let _ = stdout.flush();
}

fn install_panic_restore() {
    PANIC_RESTORE_INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            restore_terminal();
            previous(panic);
        }));
    });
}

#[derive(Clone, Copy)]
enum Code {
    Character(char),
    Escape,
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Right,
    Left,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Copy)]
struct Key {
    code: Code,
    control: bool,
    alt: bool,
}

impl Key {
    fn plain(code: Code) -> Self {
        Self { code, control: false, alt: false }
    }
}

enum Decoded {
    Key(Key),
    Ignored,
    Pending,
}

#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
    escape_since: Option<Instant>,
}

impl Decoder {
    fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn consume(&mut self, count: usize) {
        self.bytes.drain(..count);
        self.escape_since = None;
    }

    fn escape_wait(&self, now: Instant) -> Option<Duration> {
        if self.bytes == b"\x1b" {
            self.escape_since.map(|since| ESCAPE_TIMEOUT.saturating_sub(now.saturating_duration_since(since)))
        } else {
            None
        }
    }

    fn next(&mut self, now: Instant) -> io::Result<Decoded> {
        let Some(&first) = self.bytes.first() else {
            return Ok(Decoded::Pending);
        };
        if first != 0x1b {
            return self.character(0, false);
        }
        let since = *self.escape_since.get_or_insert(now);
        if self.bytes.len() == 1 {
            if now.saturating_duration_since(since) < ESCAPE_TIMEOUT {
                return Ok(Decoded::Pending);
            }
            self.consume(1);
            return Ok(Decoded::Key(Key::plain(Code::Escape)));
        }
        match self.bytes[1] {
            0x1b => {
                self.consume(1);
                Ok(Decoded::Key(Key::plain(Code::Escape)))
            }
            b'O' => {
                if self.bytes.len() < 3 {
                    return Ok(Decoded::Pending);
                }
                let code = navigation(self.bytes[2]);
                self.consume(3);
                Ok(code.map_or(Decoded::Ignored, |code| Decoded::Key(Key::plain(code))))
            }
            b'[' => self.csi(),
            _ => self.character(1, true),
        }
    }

    fn character(&mut self, offset: usize, alt: bool) -> io::Result<Decoded> {
        let byte = self.bytes[offset];
        let (mut key, length) = match byte {
            b'\r' => (Key::plain(Code::Enter), 1),
            b'\t' => (Key::plain(Code::Tab), 1),
            0x7f => (Key::plain(Code::Backspace), 1),
            0x01..=0x1a => (Key { code: Code::Character((byte + b'a' - 1) as char), control: true, alt: false }, 1),
            0x20..=0x7e => (Key::plain(Code::Character(byte as char)), 1),
            0x00 | 0x1c..=0x1f => {
                self.consume(offset + 1);
                return Ok(Decoded::Ignored);
            }
            _ => {
                let length = match byte {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid terminal UTF-8")),
                };
                if self.bytes.len() < offset + length {
                    return Ok(Decoded::Pending);
                }
                let value = std::str::from_utf8(&self.bytes[offset..offset + length])
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .chars().next().expect("UTF-8 character has at least two bytes");
                (Key::plain(Code::Character(value)), length)
            }
        };
        key.alt = alt;
        self.consume(offset + length);
        Ok(Decoded::Key(key))
    }

    fn csi(&mut self) -> io::Result<Decoded> {
        let Some(end) = self.bytes[2..].iter().position(|byte| (0x40..=0x7e).contains(byte)).map(|end| end + 2) else {
            if self.bytes.len() > 64 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "terminal escape sequence too long"));
            }
            return Ok(Decoded::Pending);
        };
        let body = &self.bytes[2..end];
        let final_byte = self.bytes[end];
        let code = if body.is_empty() {
            navigation(final_byte)
        } else if body.iter().all(|byte| byte.is_ascii_digit() || *byte == b';') {
            let first = body.split(|byte| *byte == b';').next().unwrap();
            match final_byte {
                b'~' => match first {
                    b"1" | b"7" => Some(Code::Home),
                    b"4" | b"8" => Some(Code::End),
                    b"5" => Some(Code::PageUp),
                    b"6" => Some(Code::PageDown),
                    _ => None,
                },
                _ => navigation(final_byte),
            }
        } else {
            None
        };
        self.consume(end + 1);
        Ok(code.map_or(Decoded::Ignored, |code| Decoded::Key(Key::plain(code))))
    }
}

fn navigation(byte: u8) -> Option<Code> {
    match byte {
        b'A' => Some(Code::Up),
        b'B' => Some(Code::Down),
        b'C' => Some(Code::Right),
        b'D' => Some(Code::Left),
        b'H' => Some(Code::Home),
        b'F' => Some(Code::End),
        _ => None,
    }
}

fn map_key(key: Key, text_input: bool) -> Option<Intent> {
    if text_input {
        return match key.code {
            Code::Escape => Some(Intent::Cancel),
            Code::Enter => Some(Intent::SubmitFilter),
            Code::Backspace => Some(Intent::FilterBackspace),
            Code::Character(value) if !key.control && !key.alt => Some(Intent::FilterCharacter(value)),
            _ => None,
        };
    }
    match key.code {
        Code::Character('c') if key.control => Some(Intent::Quit),
        Code::Up | Code::Character('k') => Some(Intent::MoveUp),
        Code::Down | Code::Character('j') => Some(Intent::MoveDown),
        Code::PageUp => Some(Intent::PageUp),
        Code::PageDown => Some(Intent::PageDown),
        Code::Home => Some(Intent::Home),
        Code::End => Some(Intent::End),
        Code::Enter | Code::Right | Code::Character('l') => Some(Intent::Enter),
        Code::Left | Code::Backspace | Code::Character('h') => Some(Intent::Parent),
        Code::Tab => Some(Intent::SwitchPane),
        Code::Character('d') => Some(Intent::ToggleMark),
        Code::Character('-') => Some(Intent::ToggleSplit),
        Code::Character('r') if key.control => Some(Intent::Delete),
        Code::Character('v') => Some(Intent::ToggleRange),
        Code::Character('*') => Some(Intent::MarkAll),
        Code::Character('c') => Some(Intent::ClearMarks),
        Code::Character('r') => Some(Intent::Refresh),
        Code::Character('a') => Some(Intent::ToggleSizeMode),
        Code::Character('s') => Some(Intent::ToggleSort),
        Code::Character('/') => Some(Intent::StartFilter),
        Code::Character('?') => Some(Intent::Help),
        Code::Escape => Some(Intent::Cancel),
        Code::Character('q') => Some(Intent::Quit),
        Code::Character(value) if !key.control && !key.alt => Some(Intent::FilterCharacter(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(bytes: &[u8]) -> Key {
        let mut decoder = Decoder::default();
        decoder.feed(bytes);
        match decoder.next(Instant::now()).unwrap() {
            Decoded::Key(key) => key,
            _ => panic!("expected a complete key"),
        }
    }

    #[test]
    fn navigation_and_controls_keep_their_commands() {
        for sequence in [b"\x1b[A".as_slice(), b"\x1bOA", b"\x1b[1;5A"] {
            assert!(matches!(map_key(key(sequence), false), Some(Intent::MoveUp)));
        }
        assert!(matches!(map_key(key(b"\x03"), false), Some(Intent::Quit)));
        assert!(matches!(map_key(key(b"\x12"), false), Some(Intent::Delete)));
        assert!(matches!(map_key(key(b"r"), false), Some(Intent::Refresh)));
        assert!(matches!(map_key(key(b"\t"), false), Some(Intent::SwitchPane)));
        assert!(matches!(map_key(key(b"\x1b[5~"), false), Some(Intent::PageUp)));
        assert!(matches!(map_key(key(b"\x1b[4~"), false), Some(Intent::End)));
    }

    #[test]
    fn filtering_accepts_unicode_but_rejects_alt_and_control_text() {
        assert!(matches!(map_key(key("日".as_bytes()), true), Some(Intent::FilterCharacter('日'))));
        assert!(matches!(map_key(key(b"q"), true), Some(Intent::FilterCharacter('q'))));
        assert!(map_key(key(b"\x1bq"), true).is_none());
        assert!(map_key(key(b"\x03"), true).is_none());
        assert!(matches!(map_key(key(b"\x7f"), true), Some(Intent::FilterBackspace)));
        assert!(matches!(map_key(key(b"\r"), true), Some(Intent::SubmitFilter)));
        assert!(matches!(map_key(key(b"\x1bj"), false), Some(Intent::MoveDown)));
        assert!(map_key(key(b"\x1bx"), false).is_none());
    }

    #[test]
    fn raw_linefeed_and_ctrl_h_do_not_submit_or_edit_the_filter() {
        assert!(matches!(map_key(key(b"\x0a"), false), Some(Intent::MoveDown)));
        assert!(matches!(map_key(key(b"\x08"), false), Some(Intent::Parent)));
        assert!(map_key(key(b"\x0a"), true).is_none());
        assert!(map_key(key(b"\x08"), true).is_none());
    }

    #[test]
    fn incomplete_sequences_and_utf8_survive_later_reads() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b[1;");
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Pending));
        decoder.feed(b"5Dj");
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Key(Key { code: Code::Left, .. })));
        assert!(matches!(map_key(match decoder.next(now).unwrap() {
            Decoded::Key(key) => key,
            _ => panic!("expected buffered key"),
        }, false), Some(Intent::MoveDown)));
        decoder.feed(&"é".as_bytes()[..1]);
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Pending));
        decoder.feed(&"é".as_bytes()[1..]);
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Key(Key { code: Code::Character('é'), .. })));
    }

    #[test]
    fn bare_escape_waits_for_its_timeout_and_double_escape_keeps_both() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b");
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Pending));
        assert!(matches!(decoder.next(now + ESCAPE_TIMEOUT).unwrap(), Decoded::Key(Key { code: Code::Escape, .. })));
        decoder.feed(b"\x1b\x1b");
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Key(Key { code: Code::Escape, .. })));
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Pending));
        assert!(matches!(decoder.next(now + ESCAPE_TIMEOUT).unwrap(), Decoded::Key(Key { code: Code::Escape, .. })));
    }

    #[test]
    fn unknown_terminal_sequences_do_not_become_filter_characters() {
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b[15~x");
        let now = Instant::now();
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Ignored));
        assert!(matches!(decoder.next(now).unwrap(), Decoded::Key(Key { code: Code::Character('x'), .. })));
        decoder.feed(&[0xff]);
        assert_eq!(decoder.next(now).err().unwrap().kind(), io::ErrorKind::InvalidData);
    }
}

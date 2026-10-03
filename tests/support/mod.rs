//! Shared harness for the terminal tests: temporary trees, a PTY session around
//! the real binary, and screen-text helpers.
#![allow(dead_code)]

use ptytest::{CommandSpec, ExitStatus, Key, PtyTest, Scenario, ScreenSnapshot, Size, TerminalBaseline, TestEnv};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
pub const STEP: Duration = Duration::from_secs(15);

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> io::Result<Self> {
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("fdu-pty-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// A fixture root with two subdirectories and a few files, named so that
    /// name order is deterministic: `alpha/`, `beta/`, `top.txt`.
    pub fn standard_tree() -> io::Result<(Self, PathBuf)> {
        let temp = Self::new()?;
        let root = temp.0.join("root");
        fs::create_dir_all(root.join("alpha"))?;
        fs::create_dir_all(root.join("beta"))?;
        fs::write(root.join("alpha/inner.txt"), vec![b'a'; 3000])?;
        fs::write(root.join("alpha/other.txt"), b"other")?;
        fs::write(root.join("beta/deep.txt"), vec![b'b'; 9000])?;
        fs::write(root.join("top.txt"), vec![b't'; 5000])?;
        Ok((temp, root))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Tests may leave unreadable directories behind on purpose.
        let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Session {
    pub terminal: PtyTest,
    pub baseline: TerminalBaseline,
    /// Per-operation deadline; large trees need longer than the default.
    pub step: Duration,
}

impl Session {
    pub fn start(label: &str, root: &Path, args: &[&str], columns: u16, rows: u16) -> Self {
        Self::start_with_env(label, root, args, columns, rows, &[])
    }

    pub fn start_with_env(label: &str, root: &Path, args: &[&str], columns: u16, rows: u16, env: &[(&str, &str)]) -> Self {
        let mut command = CommandSpec::new(env!("CARGO_BIN_EXE_fdu")).arg("--interactive").arg(root).args(args);
        for (key, value) in env {
            command = command.env(key, value);
        }
        let scenario = Scenario::new(label)
            .unwrap()
            .command(command)
            .size(Size::new(columns, rows).unwrap())
            .environment(TestEnv::hermetic().unwrap());
        let terminal = PtyTest::spawn(scenario).unwrap();
        let baseline = terminal.terminal_baseline();
        Self { terminal, baseline, step: STEP }
    }

    pub fn wait(&mut self, description: &str, predicate: impl Fn(&ScreenSnapshot) -> bool) -> ScreenSnapshot {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.wait_for_screen(deadline, description, predicate).unwrap();
        // One repaint can arrive in several reads, so the matching screen may
        // be mid-frame. Return the screen once the frame has finished.
        let deadline = self.terminal.deadline(self.step);
        assert!(self.terminal.wait_for_quiescence(deadline, Duration::from_millis(25)).unwrap());
        self.terminal.screen()
    }

    pub fn wait_for(&mut self, text: &str) -> ScreenSnapshot {
        self.wait(text, |screen| screen.shows(text))
    }

    pub fn wait_until_absent(&mut self, text: &str) -> ScreenSnapshot {
        self.wait(&format!("{text} to disappear"), |screen| !screen.shows(text))
    }

    /// The status strip leads with `N entries` once the scan has finished and
    /// `N entries…` while it is still adding to the count.
    pub fn wait_ready(&mut self) -> ScreenSnapshot {
        self.wait("scan to finish", |screen| {
            let status = screen.lines().pop().unwrap_or_default();
            let status = status.trim_start();
            status.split_whitespace().next().is_some_and(|count| count.parse::<usize>().is_ok())
                && status.contains(" entries ")
        })
    }

    /// Waits for the scan to settle, then sorts by name and returns to the first
    /// row so the tests do not depend on filesystem block sizes.
    pub fn ready_by_name(&mut self) {
        self.wait_ready();
        self.text("s");
        self.wait_for("Sorted by name");
        self.key(Key::Home);
    }

    pub fn text(&mut self, text: &str) {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.send_text(deadline, text).unwrap();
    }

    pub fn key(&mut self, key: Key) {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.send_key(deadline, key).unwrap();
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.terminal.resize(Size::new(columns, rows).unwrap()).unwrap();
    }

    /// Quits and checks the process exits cleanly and gives the terminal back.
    pub fn quit(self) {
        self.quit_with_output();
    }

    /// Like [`quit`](Self::quit), returning everything the program wrote, which
    /// includes anything it printed after leaving the alternate screen.
    pub fn quit_with_output(mut self) -> String {
        self.text("q");
        let deadline = self.terminal.deadline(self.step);
        let status = self.terminal.wait_for_exit(deadline).unwrap();
        assert_eq!(status, ExitStatus::Code(0));
        self.terminal.assert_terminal_restored(&self.baseline).unwrap();
        let output = String::from_utf8_lossy(self.terminal.raw_output()).into_owned();
        let deadline = self.terminal.deadline(self.step);
        self.terminal.finish(deadline).unwrap();
        output
    }
}

pub trait ScreenText {
    fn lines(&self) -> Vec<String>;
    fn shows(&self, needle: &str) -> bool;
}

impl ScreenText for ScreenSnapshot {
    fn lines(&self) -> Vec<String> {
        (0..self.row_count()).map(|row| self.row(row).unwrap_or_default()).collect()
    }

    fn shows(&self, needle: &str) -> bool {
        self.contains(needle)
    }
}

/// The list's frame is drawn dimmed whenever the marked pane has focus. Only
/// meaningful while both panes are on screen.
pub fn list_is_dimmed(screen: &ScreenSnapshot) -> bool {
    screen.cell(1, 0).is_some_and(|cell| cell.attributes().dim)
}

pub fn row_containing(screen: &ScreenSnapshot, needle: &str) -> String {
    screen
        .lines()
        .into_iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row contains {needle:?}"))
}


/// Where the generated layout fixture lives. The path is fixed because the root
/// path appears on screen, and screen snapshots must not depend on the machine.
pub const LAYOUT_FIXTURE: &str = "/tmp/fdu-layout-fixture";
const LAYOUT_FIXTURE_MARKER: &str = "/tmp/fdu-layout-fixture.ready";
/// What a complete generation looks like: the generator's seed and entry count.
const LAYOUT_FIXTURE_DESCRIPTION: &str = "seed=0 entries=192238";

/// The anonymized workspace-layout tree (about 192 thousand entries), generated
/// once with a fixed seed and reused across runs. Never mutate it; use
/// [`clone_layout_fixture`] for tests that delete.
pub fn layout_fixture() -> &'static Path {
    static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let marker = fs::read_to_string(LAYOUT_FIXTURE_MARKER).unwrap_or_default();
        if marker.trim() == LAYOUT_FIXTURE_DESCRIPTION && Path::new(LAYOUT_FIXTURE).is_dir() {
            return;
        }
        let _ = fs::remove_file(LAYOUT_FIXTURE_MARKER);
        let partial = format!("{LAYOUT_FIXTURE}.partial");
        let _ = fs::remove_dir_all(&partial);
        let _ = fs::remove_dir_all(LAYOUT_FIXTURE);
        let output = std::process::Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/generate_layout_fixture.py"))
            .arg(&partial)
            .output()
            .expect("python3 generates the layout fixture");
        assert!(output.status.success(), "fixture generation failed: {}", String::from_utf8_lossy(&output.stderr));
        let report = String::from_utf8_lossy(&output.stdout);
        assert!(report.contains("entries=192238"), "unexpected fixture shape: {report}");
        fs::rename(&partial, LAYOUT_FIXTURE).unwrap();
        fs::write(LAYOUT_FIXTURE_MARKER, LAYOUT_FIXTURE_DESCRIPTION).unwrap();
    });
    Path::new(LAYOUT_FIXTURE)
}

/// A private copy of the layout fixture inside `temp`, made with APFS
/// copy-on-write cloning so it is cheap. Destructive tests work on the copy.
pub fn clone_layout_fixture(temp: &TempDir) -> PathBuf {
    let destination = temp.0.join("layout");
    let status = std::process::Command::new("cp")
        .args(["-cR"])
        .arg(layout_fixture())
        .arg(&destination)
        .status()
        .expect("cp clones the layout fixture");
    assert!(status.success(), "cloning the layout fixture failed");
    destination
}

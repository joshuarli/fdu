#![cfg(all(target_os = "macos", feature = "interactive"))]

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> io::Result<Self> {
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Pty {
    master: fs::File,
    slave: fs::File,
    slave_path: PathBuf,
    initial_flags: libc::tcflag_t,
}

impl Pty {
    fn new() -> io::Result<Self> {
        let mut master = -1;
        let mut slave = -1;
        let mut name = [0 as libc::c_char; 128];
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                &mut size,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let slave_name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes().to_vec();
        let slave_path = PathBuf::from(std::ffi::OsString::from_vec(slave_name));
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::zeroed();
        if unsafe { libc::tcgetattr(slave.as_raw_fd(), attributes.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let initial_flags = unsafe { attributes.assume_init() }.c_lflag;
        Ok(Self { master, slave, slave_path, initial_flags })
    }

    fn spawn(&self, root: &std::path::Path, read_only: bool) -> io::Result<Child> {
        self.spawn_with_profile(root, read_only, false)
    }

    fn spawn_with_profile(
        &self,
        root: &std::path::Path,
        read_only: bool,
        profile: bool,
    ) -> io::Result<Child> {
        let input = self.slave.try_clone()?;
        let output = self.slave.try_clone()?;
        let error = self.slave.try_clone()?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_fdu"));
        command
            .arg("--interactive")
            .arg(root)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error));
        if profile {
            command.env("FDU_PROFILE", "1");
        }
        if read_only {
            command.arg("--read-only");
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::tcsetpgrp(0, libc::getpgrp()) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn()
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)
    }

    fn wait_for(&mut self, child: &mut Child, needle: &[u8]) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while Instant::now() < deadline {
            loop {
                match self.master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => return Err(error),
                }
            }
            if output.windows(needle.len()).any(|window| window == needle) {
                return Ok(output);
            }
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("fdu exited before drawing the expected text: {status}")));
            }
            thread::sleep(Duration::from_millis(10));
        }
        let start = output.len().saturating_sub(600);
        let _ = child.kill();
        let _ = child.wait();
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for {:?}; terminal output tail: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&output[start..])
            ),
        ))
    }

    fn wait_for_exit(&mut self, child: &mut Child) -> io::Result<std::process::ExitStatus> {
        self.wait_for_exit_output(child).map(|(status, _)| status)
    }

    fn wait_for_exit_output(
        &mut self,
        child: &mut Child,
    ) -> io::Result<(std::process::ExitStatus, Vec<u8>)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(status) = child.try_wait()? {
                loop {
                    match self.master.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => output.extend_from_slice(&buffer[..count]),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(error) => return Err(error),
                    }
                }
                return Ok((status, output));
            }
            match self.master.read(&mut buffer) {
                Ok(0) => {}
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "fdu did not exit after quit input"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn resize_and_signal(&self, child: &Child) -> io::Result<()> {
        let mut size = libc::winsize {
            ws_row: 10,
            ws_col: 36,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &mut size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGWINCH) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn assert_restored(&self) -> io::Result<()> {
        let slave = fs::OpenOptions::new().read(true).write(true).open(&self.slave_path)?;
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::zeroed();
        if unsafe { libc::tcgetattr(slave.as_raw_fd(), attributes.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let current_flags = unsafe { attributes.assume_init() }.c_lflag;
        let restore_mask = libc::ICANON | libc::ECHO | libc::ISIG;
        assert_eq!(current_flags & restore_mask, self.initial_flags & restore_mask);
        Ok(())
    }

}

#[test]
fn interactive_navigation_cancel_resize_and_ctrl_c_restore_terminal_state() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    let child_dir = root.join("child");
    fs::create_dir(&child_dir)?;
    let payload = child_dir.join("payload");
    fs::write(&payload, b"keep this file")?;

    let mut pty = Pty::new()?;
    let mut child = pty.spawn(&root, false)?;
    pty.wait_for(&mut child, b"Ready")?;
    pty.send(b"\r")?;
    pty.wait_for(&mut child, b"child")?;
    pty.send(b"d")?;
    pty.wait_for(&mut child, b"Permanently delete these selected entries?")?;
    pty.send(&[0x1b])?;
    thread::sleep(Duration::from_millis(100));
    pty.resize_and_signal(&child)?;
    pty.send(&[0x03])?;
    let status = pty.wait_for_exit(&mut child)?;
    assert!(status.success());
    assert!(payload.exists(), "cancelling confirmation must leave the fixture unchanged");
    pty.assert_restored()?;
    Ok(())
}

#[test]
fn interactive_read_only_session_rejects_delete_and_restores_terminal_state() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    let payload = root.join("payload");
    fs::write(&payload, b"keep this file")?;

    let mut pty = Pty::new()?;
    let mut child = pty.spawn(&root, true)?;
    pty.wait_for(&mut child, b"Ready")?;
    pty.send(b"d")?;
    pty.wait_for(&mut child, b"read only")?;
    pty.send(b"q")?;
    let status = pty.wait_for_exit(&mut child)?;
    assert!(status.success());
    assert!(payload.exists(), "read-only input must not remove fixture entries");
    pty.assert_restored()?;
    Ok(())
}

#[test]
fn interactive_renders_first_listing_before_scan_settles() -> io::Result<()> {
    const DIRECTORIES: usize = 512;

    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    for index in 0..DIRECTORIES {
        let child = root.join(format!("child-{index:03}"));
        fs::create_dir(&child)?;
        fs::write(child.join("payload"), b"x")?;
    }

    let mut pty = Pty::new()?;
    let mut child = pty.spawn_with_profile(&root, true, true)?;
    pty.wait_for(&mut child, b"Ready")?;
    pty.send(b"q")?;
    let (status, output) = pty.wait_for_exit_output(&mut child)?;
    assert!(status.success(), "fdu exited unsuccessfully: {status}");
    pty.assert_restored()?;

    let output = String::from_utf8_lossy(&output);
    let profile_field = |name: &str| {
        output
            .split_whitespace()
            .find_map(|field| field.strip_prefix(name))
    };
    let first_listing_ms: u128 = profile_field("first_usable_listing_ms=")
        .expect("profile output must report the first usable listing time")
        .parse()
        .expect("listing time must be an integer number of milliseconds");
    let settled_ms: u128 = profile_field("initial_scan_settled_ms=")
        .expect("profile output must report the initial scan settle time")
        .parse()
        .expect("settle time must be an integer number of milliseconds");
    assert!(
        first_listing_ms < settled_ms,
        "the root listing should be drawn before its nested scan completes; first listing {first_listing_ms} ms, settled {settled_ms} ms"
    );
    Ok(())
}

#[test]
#[ignore = "host-dependent release-tree timing; run cargo test --release --test interactive_pty release_artifact_tree_reaches_ready_under_100ms -- --ignored --exact"]
fn release_artifact_tree_reaches_ready_under_100ms() -> io::Result<()> {
    const RUNS: usize = 5;
    const MIN_INDEXED_ENTRIES: usize = 1_000;

    let release_tree = PathBuf::from(env!("CARGO_BIN_EXE_fdu"))
        .parent()
        .expect("the fdu binary must be inside target/release")
        .to_path_buf();
    let mut samples = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        let mut pty = Pty::new()?;
        let mut child = pty.spawn_with_profile(&release_tree, true, true)?;
        pty.wait_for(&mut child, b"Ready")?;
        pty.send(b"q")?;
        let (status, output) = pty.wait_for_exit_output(&mut child)?;
        assert!(status.success(), "fdu exited unsuccessfully: {status}");
        pty.assert_restored()?;

        let output = String::from_utf8_lossy(&output);
        let profile_field = |name: &str| {
            output
                .split_whitespace()
                .find_map(|field| field.strip_prefix(name))
        };
        let settled_ms: u128 = profile_field("initial_scan_settled_ms=")
            .expect("profile output must report the initial scan settle time")
            .parse()
            .expect("settle time must be an integer number of milliseconds");
        let entries: usize = profile_field("entries=")
            .expect("profile output must report the indexed entry count")
            .parse()
            .expect("indexed entry count must be an integer");
        assert!(
            entries >= MIN_INDEXED_ENTRIES,
            "expected a populated release tree with at least {MIN_INDEXED_ENTRIES} entries, got {entries}"
        );
        samples.push(settled_ms);
    }

    samples.sort_unstable();
    let median_ms = samples[samples.len() / 2];
    eprintln!("initial scan settle samples: {samples:?} ms; median: {median_ms} ms");
    assert!(
        median_ms < 100,
        "initial scan settle median must be below 100 ms; samples: {samples:?}"
    );
    Ok(())
}

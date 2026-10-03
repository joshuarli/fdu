use fdu::{scan, ScanOptions, ScanReport};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> io::Result<Self> {
        loop {
            let id = TEMP_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fdu-integration-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scan_path(path: &Path, apparent: bool) -> io::Result<ScanReport> {
    scan(&ScanOptions {
        path: path.to_path_buf(),
        apparent,
    })
}

fn allocated_size(path: &Path) -> io::Result<u64> {
    fs::symlink_metadata(path)?
        .blocks()
        .checked_mul(512)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "test size overflow"))
}

fn reported_size(report: &ScanReport, name: &str) -> u64 {
    report
        .directories
        .iter()
        .find(|directory| directory.name == OsStr::new(name))
        .unwrap_or_else(|| panic!("top-level directory {name:?} is present"))
        .disk_size
}

fn assert_expected_diagnostics(stderr: &[u8]) {
    if cfg!(feature = "allocation-profile") {
        let diagnostics = std::str::from_utf8(stderr).unwrap();
        assert!(diagnostics.starts_with("fdu-allocations "));
        assert_eq!(diagnostics.lines().count(), 1);
    } else {
        assert!(stderr.is_empty());
    }
}

#[test]
fn accounts_for_nested_files_links_sparse_files_and_raw_names() -> io::Result<()> {
    let temporary = TemporaryDirectory::new()?;
    let root = &temporary.0;
    let included = root.join("included");
    let nested = included.join("nested");
    let empty = root.join("empty");
    fs::create_dir(&included)?;
    fs::create_dir(&nested)?;
    fs::create_dir(&empty)?;

    let payload = included.join("payload.bin");
    let hardlink = included.join("payload-hardlink.bin");
    let sparse = included.join("sparse.bin");
    let unusual_name: &[u8] = if cfg!(target_os = "macos") {
        b"unusual-\xc3\xa9-\n-name"
    } else {
        b"invalid-\xff-name"
    };
    let unusual = included.join(OsString::from_vec(unusual_name.to_vec()));
    let file_link = included.join("payload-link");
    let dangling_link = included.join("dangling-link");
    let directory_link = included.join("nested-link");
    let nested_payload = nested.join("nested.bin");

    fs::write(&payload, vec![0x5a; 8192])?;
    fs::hard_link(&payload, &hardlink)?;
    File::create(&sparse)?.set_len(1024 * 1024)?;
    fs::write(&unusual, b"raw filename bytes")?;
    symlink("payload.bin", &file_link)?;
    symlink("missing-target", &dangling_link)?;
    symlink("nested", &directory_link)?;
    fs::write(&nested_payload, vec![0x31; 257])?;
    File::create(included.join("empty-file"))?;
    File::create(root.join("root-file"))?.write_all(b"excluded")?;
    symlink("included", root.join("root-directory-link"))?;

    let allocated = [
        payload.as_path(),
        hardlink.as_path(),
        sparse.as_path(),
        unusual.as_path(),
        file_link.as_path(),
        dangling_link.as_path(),
        directory_link.as_path(),
        nested_payload.as_path(),
    ]
    .into_iter()
    .try_fold(0u64, |total, path| {
        total
            .checked_add(allocated_size(path)?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "test size overflow"))
    })?;
    let apparent = [
        payload.as_path(),
        hardlink.as_path(),
        sparse.as_path(),
        unusual.as_path(),
        file_link.as_path(),
        dangling_link.as_path(),
        directory_link.as_path(),
        nested_payload.as_path(),
    ]
    .into_iter()
    .try_fold(0u64, |total, path| {
        total
            .checked_add(fs::symlink_metadata(path)?.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "test size overflow"))
    })?;

    let report = scan_path(root, false)?;
    assert_eq!(report.skipped_entries, 0);
    assert_eq!(report.mount_boundaries, 0);
    assert_eq!(report.directories.len(), 2);
    assert_eq!(report.directories[0].name, OsStr::new("included"));
    assert_eq!(reported_size(&report, "included"), allocated);
    assert_eq!(reported_size(&report, "empty"), 0);

    let report = scan_path(root, true)?;
    assert_eq!(report.skipped_entries, 0);
    assert_eq!(report.mount_boundaries, 0);
    assert_eq!(report.directories.len(), 2);
    assert_eq!(reported_size(&report, "included"), apparent);
    assert_eq!(reported_size(&report, "empty"), 0);
    Ok(())
}

#[test]
fn follows_a_symlink_used_as_the_command_line_root() -> io::Result<()> {
    let temporary = TemporaryDirectory::new()?;
    let actual_root = temporary.0.join("actual-root");
    let supplied_root = temporary.0.join("root-link");
    let child = actual_root.join("child");
    fs::create_dir(&actual_root)?;
    fs::create_dir(&child)?;
    fs::write(child.join("payload"), b"root target")?;
    symlink(&actual_root, &supplied_root)?;

    let report = scan_path(&supplied_root, true)?;
    assert_eq!(report.skipped_entries, 0);
    assert_eq!(report.mount_boundaries, 0);
    assert_eq!(report.directories.len(), 1);
    assert_eq!(reported_size(&report, "child"), 11);
    Ok(())
}

#[test]
fn handles_wide_and_deep_trees_without_path_or_stack_recursion() -> io::Result<()> {
    const FILE_COUNT: usize = 2050;
    const NAME_LENGTH: usize = 250;
    const BRANCH_COUNT: usize = 128;
    const DEEP_LEVELS: usize = 160;

    let temporary = TemporaryDirectory::new()?;
    let wide = temporary.0.join("wide");
    let deep = temporary.0.join("deep");
    fs::create_dir(&wide)?;
    fs::create_dir(&deep)?;

    let mut wide_allocated = 0u64;
    let mut wide_apparent = 0u64;
    for index in 0..FILE_COUNT {
        let mut name = format!("entry-{index:04}").into_bytes();
        name.resize(NAME_LENGTH, b'x');
        let path = wide.join(OsString::from_vec(name));
        fs::write(&path, b"x")?;
        wide_allocated += allocated_size(&path)?;
        wide_apparent += 1;
    }
    for index in 0..BRANCH_COUNT {
        let branch = wide.join(format!("branch-{index:04}"));
        fs::create_dir(&branch)?;
        let path = branch.join("leaf");
        fs::write(&path, b"branch")?;
        wide_allocated += allocated_size(&path)?;
        wide_apparent += 6;
    }

    let deep_payload = create_deep_chain(&deep, DEEP_LEVELS)?;
    let deep_allocated = allocated_size(&deep_payload)?;

    for (apparent, expected_wide, expected_deep) in [
        (false, wide_allocated, deep_allocated),
        (true, wide_apparent, 4),
    ] {
        let report = scan_path(&temporary.0, apparent)?;
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(report.mount_boundaries, 0);
        assert_eq!(report.directories.len(), 2);
        assert_eq!(reported_size(&report, "wide"), expected_wide);
        assert_eq!(reported_size(&report, "deep"), expected_deep);
    }
    Ok(())
}

fn create_deep_chain(root: &Path, levels: usize) -> io::Result<PathBuf> {
    let directory_name = CString::new("d").unwrap();
    let leaf_name = CString::new("leaf").unwrap();
    let mut expected_path = root.to_path_buf();
    let mut parent = File::open(root)?;

    for _ in 0..levels {
        if unsafe { libc::mkdirat(parent.as_raw_fd(), directory_name.as_ptr(), 0o700) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let child_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                directory_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if child_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        parent = unsafe { File::from_raw_fd(child_fd) };
        expected_path.push("d");
    }

    let leaf_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    if leaf_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut leaf = unsafe { File::from_raw_fd(leaf_fd) };
    leaf.write_all(b"deep")?;
    expected_path.push("leaf");
    Ok(expected_path)
}

#[test]
fn reports_scan_errors_and_partial_permission_failures() -> io::Result<()> {
    let temporary = TemporaryDirectory::new()?;
    let missing = temporary.0.join("missing");
    assert!(scan_path(&missing, false).is_err());

    let top = temporary.0.join("top");
    let blocked = top.join("blocked");
    let good_file = top.join("good-file");
    let blocked_file = blocked.join("blocked-file");
    fs::create_dir(&top)?;
    fs::create_dir(&blocked)?;
    fs::write(&good_file, b"good")?;
    fs::write(&blocked_file, b"blocked")?;
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0))?;

    let result = scan_path(&temporary.0, false);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700))?;
    let report = result?;
    assert_eq!(report.mount_boundaries, 0);
    if unsafe { libc::geteuid() } == 0 {
        assert_eq!(report.skipped_entries, 0);
        assert_eq!(
            reported_size(&report, "top"),
            allocated_size(&good_file)? + allocated_size(&blocked_file)?
        );
    } else {
        assert!(report.skipped_entries >= 1);
        assert_eq!(reported_size(&report, "top"), allocated_size(&good_file)?);
    }
    Ok(())
}

#[test]
fn prints_only_immediate_directory_totals_in_both_size_modes() -> io::Result<()> {
    let temporary = TemporaryDirectory::new()?;
    let root = &temporary.0;
    let alpha = root.join("alpha");
    let nested = alpha.join("nested");
    let empty = root.join("empty");
    fs::create_dir(&alpha)?;
    fs::create_dir(&nested)?;
    fs::create_dir(&empty)?;

    File::create(alpha.join("zero"))?;
    let mut small = File::create(alpha.join("small"))?;
    small.write_all(b"abc")?;
    let mut nested_small = File::create(nested.join("nested-small"))?;
    nested_small.write_all(b"12345")?;
    File::create(root.join("root-file"))?.write_all(b"not listed")?;

    let allocated = allocated_size(&alpha.join("small"))?
        + allocated_size(&nested.join("nested-small"))?;
    let expected_allocated = format!("{allocated}\t\"alpha\"\n0\t\"empty\"\n");
    let output = Command::new(env!("CARGO_BIN_EXE_fdu"))
        .arg(root)
        .output()?;
    assert!(output.status.success());
    assert_expected_diagnostics(&output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected_allocated);

    let output = Command::new(env!("CARGO_BIN_EXE_fdu"))
        .arg("--apparent")
        .arg(root)
        .output()?;
    assert!(output.status.success());
    assert_expected_diagnostics(&output.stderr);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "8\t\"alpha\"\n0\t\"empty\"\n"
    );
    Ok(())
}

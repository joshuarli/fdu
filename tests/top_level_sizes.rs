use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
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

fn allocated_size(path: &Path) -> io::Result<u64> {
    Ok(fs::symlink_metadata(path)?.blocks() * 512)
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
    let mut root_file = File::create(root.join("root-file"))?;
    root_file.write_all(b"not listed")?;

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

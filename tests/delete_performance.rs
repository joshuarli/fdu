//! Deleting the whole generated layout fixture through the interface. The
//! destructive work happens on a private copy-on-write clone of the fixture, so
//! the shared fixture is untouched. The test is ignored by default because it is
//! slow; run it with `--ignored --nocapture` (and `--release` for meaningful
//! numbers) to see the timing and the app's own deletion profile.
#![cfg(feature = "interactive")]

mod support;

use ptytest::Key;
use std::fs;
use std::time::{Duration, Instant};
use support::{clone_layout_fixture, Session, TempDir};

#[test]
#[ignore = "deletes about 190 thousand entries; run cargo test --release --test delete_performance -- --ignored --nocapture"]
fn deleting_the_whole_layout_fixture_removes_every_entry() {
    let temp = TempDir::new().unwrap();
    let root = clone_layout_fixture(&temp);
    let mut session = Session::start_with_env("delete-layout", &root, &[], 100, 24, &[("FDU_PROFILE", "1")]);
    session.step = Duration::from_secs(300);
    session.wait_ready();

    // `*` marks every entry in the root; the plan then covers the whole tree.
    session.text("*");
    session.wait_for("Marked 48 items");
    session.key(Key::Tab);
    session.key(Key::Ctrl('r'));
    session.wait_for("Permanently delete 48 marked items");

    let started = Instant::now();
    session.key(Key::Enter);
    let screen = session.wait("deletion summary", |screen| screen.contains("Deleted "));
    let elapsed = started.elapsed();
    let summary = (0..screen.row_count()).filter_map(|row| screen.row(row)).find(|row| row.contains("Deleted ")).unwrap();
    eprintln!("deleted the layout fixture in {elapsed:?}: {}", summary.trim());

    assert!(summary.contains("Deleted 192238 · 0 already absent · 0 changed · 0 failed · 0 not attempted"), "{summary}");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0, "every entry under the clone is gone");
    assert!(support::layout_fixture().join("directory-000000").exists(), "the shared fixture is untouched");
    let output = session.quit_with_output();
    if let Some(profile) = output.lines().find_map(|line| line.find("fdu-profile").map(|at| &line[at..])) {
        eprintln!("{profile}");
    }
}

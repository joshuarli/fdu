//! Frozen screens of the interactive browser, recorded from the generated
//! layout fixture with cell attributes so layout, dimming, and reverse video
//! are all pinned. Sizes are apparent bytes, which do not depend on the
//! filesystem's block size. macOS only.
//!
//! After an intended UI change, review the diff and re-record with
//! `PTYTEST_UPDATE_SNAPSHOTS=1 cargo test --test ui_snapshots`.
#![cfg(all(target_os = "macos", feature = "interactive"))]

mod support;

use ptytest::{Key, SnapshotOptions};
use std::path::PathBuf;
use std::time::Duration;
use support::{layout_fixture, Session};

/// Opening the 192 thousand entry tree takes far longer than the small trees
/// the behavior tests use.
const SCAN_STEP: Duration = Duration::from_secs(120);

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(format!("{name}.ptytest"))
}

fn open(label: &str, columns: u16, rows: u16) -> Session {
    let mut session = Session::start(label, layout_fixture(), &["--apparent", "--read-only"], columns, rows);
    session.step = SCAN_STEP;
    session.wait_ready();
    session
}

fn freeze(session: &mut Session, name: &str) {
    session
        .terminal
        .assert_snapshot_with(golden(name), SnapshotOptions::default().with_attributes())
        .unwrap();
}

#[test]
fn ready_listing() {
    let mut session = open("snapshot-ready", 100, 24);
    freeze(&mut session, "ready-100x24");
    session.quit();
}

#[test]
fn marked_pane_beside_the_list() {
    let mut session = open("snapshot-marked", 100, 24);
    session.text("d");
    session.text("d");
    session.wait_for("Marked 2 items");
    freeze(&mut session, "marked-100x24");
    session.key(Key::Tab);
    session.wait("marked focus", |screen| support::list_is_dimmed(screen));
    freeze(&mut session, "marked-focus-100x24");
    session.quit();
}

#[test]
fn stacked_panes() {
    let mut session = open("snapshot-stacked", 100, 24);
    session.text("d");
    session.wait_for("Marked 1 item");
    session.text("-");
    session.wait("stacked", |screen| {
        let row = |needle: &str| (0..screen.row_count()).find(|row| screen.row(*row).is_some_and(|text| text.contains(needle)));
        matches!((row("directory-"), row("Marked 1 item")), (Some(list), Some(marked)) if list < marked)
    });
    freeze(&mut session, "stacked-100x24");
    session.quit();
}

#[test]
fn narrow_terminal_shows_one_pane() {
    let mut session = open("snapshot-narrow", 50, 24);
    session.text("d");
    session.text("d");
    session.key(Key::Tab);
    session.wait_for("Marked 2 items");
    freeze(&mut session, "narrow-marked-50x24");
    session.quit();
}

#[test]
fn inside_a_marked_directory() {
    let mut session = open("snapshot-covered", 100, 24);
    session.text("d");
    session.key(Key::Home);
    session.key(Key::Enter);
    session.wait_for("Inside marked directory");
    freeze(&mut session, "covered-100x24");
    session.quit();
}

#[test]
fn help_overlay() {
    let mut session = open("snapshot-help", 100, 24);
    session.text("?");
    session.wait_for("closes help");
    freeze(&mut session, "help-100x24");
    session.key(Key::Escape);
    session.wait_until_absent("closes help");
    session.quit();
}

#[test]
fn filter_prompt_and_applied_filter() {
    let mut session = open("snapshot-filter", 100, 24);
    session.text("/");
    session.text("directory-00001");
    session.wait_for("directory-00001");
    freeze(&mut session, "filter-prompt-100x24");
    session.key(Key::Enter);
    session.wait_for("filter \"directory-00001\"");
    freeze(&mut session, "filtered-100x24");
    session.quit();
}

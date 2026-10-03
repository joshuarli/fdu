use fdu_core::{
    DirectoryToken, EntryType, NodeId, NodeState, ScanEvent, Tree,
};
use fdu_scan::{open_root, scan, start_indexed_scan, EntryBatch, RootAnchor, ScanOptions};
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
const BATCH_SIZE: usize = 1024;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> io::Result<Self> {
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("fdu-indexed-{}-{id}", std::process::id()));
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

struct IndexedResult {
    tree: Tree,
    errors: Vec<String>,
    batches: Vec<usize>,
    directory_finish_batch_sizes: Vec<usize>,
}

fn scan_to_tree(root: RootAnchor, capacity: usize) -> io::Result<IndexedResult> {
    let mut tree = Tree::new(&root.name, root.identity, root.link_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root index name too long"))?;
    let mut tokens = vec![Some(tree.root())];
    let (sender, receiver) = mpsc::sync_channel(capacity);
    let (batch_sender, batch_receiver) = mpsc::sync_channel(capacity);
    for _ in 0..capacity {
        batch_sender
            .send(EntryBatch::with_capacity(DirectoryToken(0), BATCH_SIZE, BATCH_SIZE * 24))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "batch pool closed"))?;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = start_indexed_scan(root.clone(), sender, batch_receiver, Arc::clone(&cancelled));
    let mut errors = Vec::new();
    let mut batches = Vec::new();
    let mut directory_finish_batch_sizes = Vec::new();
    let mut done = false;
    while !done {
        let event = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?;
        match event {
            ScanEvent::Started { identity, .. } => {
                assert_eq!(identity, tree.record(tree.root()).unwrap().identity);
            }
            ScanEvent::Entries(mut batch) => {
                batches.push(batch.entries.len());
                let parent = tokens
                    .get(batch.directory.0 as usize)
                    .copied()
                    .flatten()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown directory token"))?;
                let mut apparent = 0u64;
                let mut allocated = 0u64;
                for entry in &batch.entries {
                    let name = &batch.names[entry.name.start as usize..entry.name.end as usize];
                    let id = tree
                        .append(
                            parent,
                            name,
                            entry.entry_type,
                            entry.identity,
                            entry.link_count,
                            entry.apparent_bytes,
                            entry.allocated_bytes,
                            entry.state,
                        )
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "index exhausted"))?;
                    if let Some(token) = entry.directory_token {
                        if tokens.len() <= token.0 as usize {
                            tokens.resize(token.0 as usize + 1, None);
                        }
                        tokens[token.0 as usize] = Some(id);
                    }
                    if entry.entry_type != EntryType::Directory && entry.state == NodeState::Complete {
                        apparent = apparent.checked_add(entry.apparent_bytes).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "test apparent overflow")
                        })?;
                        allocated = allocated.checked_add(entry.allocated_bytes).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "test allocated overflow")
                        })?;
                    }
                    match entry.state {
                        NodeState::Incomplete => tree.mark_incomplete_to_root(parent),
                        NodeState::Excluded(reason) => {
                            tree.set_state(id, NodeState::Excluded(reason));
                            tree.mark_incomplete_to_root(parent);
                        }
                        _ => {}
                    }
                }
                assert!(tree.add_to_ancestors(parent, apparent, allocated));
                batch.entries.clear();
                batch.names.clear();
                let _ = batch_sender.try_send(batch);
            }
            ScanEvent::DirectoryFinished { directory, complete } => {
                directory_finish_batch_sizes.push(1);
                if let Some(node) = tokens.get(directory.0 as usize).copied().flatten() {
                    if complete && tree.record(node).unwrap().state == NodeState::Scanning {
                        tree.set_state(node, NodeState::Complete);
                    } else if !complete {
                        tree.mark_incomplete_to_root(node);
                    }
                }
            }
            ScanEvent::DirectoriesFinished(directories) => {
                directory_finish_batch_sizes.push(directories.len());
                for (directory, complete) in directories {
                    if let Some(node) = tokens.get(directory.0 as usize).copied().flatten() {
                        if complete && tree.record(node).unwrap().state == NodeState::Scanning {
                            tree.set_state(node, NodeState::Complete);
                        } else if !complete {
                            tree.mark_incomplete_to_root(node);
                        }
                    }
                }
            }
            ScanEvent::DirectoryExcluded { directory, reason } => {
                if let Some(node) = tokens.get(directory.0 as usize).copied().flatten() {
                    tree.set_state(node, NodeState::Excluded(reason));
                }
            }
            ScanEvent::DirectoryFailed { directory, message } => {
                errors.push(message);
                if let Some(node) = tokens.get(directory.0 as usize).copied().flatten() {
                    tree.mark_incomplete_to_root(node);
                }
            }
            ScanEvent::Failed { message } => errors.push(message),
            ScanEvent::Cancelled => errors.push("cancelled".to_owned()),
            ScanEvent::Finished => done = true,
        }
    }
    worker
        .join()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "scanner worker panicked"))?;
    Ok(IndexedResult {
        tree,
        errors,
        batches,
        directory_finish_batch_sizes,
    })
}

fn child_named(tree: &Tree, parent: NodeId, name: &[u8]) -> Option<NodeId> {
    tree.children(parent).find(|id| tree.name(*id) == Some(name))
}

fn allocated(path: &Path) -> io::Result<u64> {
    fs::symlink_metadata(path)?
        .blocks()
        .checked_mul(512)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "allocated test bytes overflow"))
}

#[test]
fn indexed_policy_keeps_both_size_modes_and_entry_identity() -> io::Result<()> {
    let temp = TempDir::new()?;
    let actual = temp.0.join("actual root");
    let supplied = temp.0.join("root-link");
    let outer = actual.join("outer");
    let nested = outer.join("nested");
    fs::create_dir(&actual)?;
    fs::create_dir(&outer)?;
    fs::create_dir(&nested)?;
    symlink(&actual, &supplied)?;

    let payload = outer.join("payload");
    let hardlink = outer.join("payload-hardlink");
    let sparse = outer.join("sparse");
    let unusual = outer.join(OsString::from_vec(b"unusual-\xc3\xa9-\n-name".to_vec()));
    let file_link = outer.join("file-link");
    let dangling_link = outer.join("dangling-link");
    let directory_link = outer.join("directory-link");
    let nested_payload = nested.join("nested-file");
    let root_file = actual.join("root-file");
    let parallel = actual.join("parallel");
    let parallel_payload = parallel.join("parallel-file");
    fs::write(&payload, b"payload data")?;
    fs::hard_link(&payload, &hardlink)?;
    File::create(&sparse)?.set_len(1 << 20)?;
    fs::write(&unusual, b"raw name")?;
    symlink("payload", &file_link)?;
    symlink("missing-target", &dangling_link)?;
    symlink("nested", &directory_link)?;
    fs::write(&nested_payload, b"nested")?;
    fs::write(&root_file, b"direct root data")?;
    fs::create_dir(&parallel)?;
    fs::write(&parallel_payload, b"parallel branch data")?;

    let mut fifo_name = CString::new(outer.as_os_str().as_bytes()).unwrap().into_bytes();
    fifo_name.push(b'/');
    fifo_name.extend_from_slice(b"fifo");
    let fifo = CString::new(fifo_name).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let root = open_root(&supplied)?;
    let result = scan_to_tree(root, 2)?;
    assert!(result.errors.is_empty(), "scanner errors: {:?}", result.errors);
    assert!(!result.batches.is_empty());
    let tree = result.tree;
    let root_id = tree.root();
    let outer_id = child_named(&tree, root_id, b"outer").unwrap();
    let root_file_id = child_named(&tree, root_id, b"root-file").unwrap();
    let parallel_id = child_named(&tree, root_id, b"parallel").unwrap();
    let parallel_payload_id = child_named(&tree, parallel_id, b"parallel-file").unwrap();
    let payload_id = child_named(&tree, outer_id, b"payload").unwrap();
    let hardlink_id = child_named(&tree, outer_id, b"payload-hardlink").unwrap();
    let sparse_id = child_named(&tree, outer_id, b"sparse").unwrap();
    let file_link_id = child_named(&tree, outer_id, b"file-link").unwrap();
    let dangling_id = child_named(&tree, outer_id, b"dangling-link").unwrap();
    let directory_link_id = child_named(&tree, outer_id, b"directory-link").unwrap();
    let fifo_id = child_named(&tree, outer_id, b"fifo").unwrap();

    assert_eq!(tree.name(root_id), Some(supplied.as_os_str().as_bytes()));
    assert_eq!(tree.record(outer_id).unwrap().state, NodeState::Complete);
    assert_eq!(tree.record(parallel_id).unwrap().state, NodeState::Complete);
    assert_eq!(tree.record(parallel_payload_id).unwrap().apparent_bytes, fs::metadata(&parallel_payload)?.len());
    assert_eq!(
        tree.record(outer_id).unwrap().link_count,
        fs::symlink_metadata(&outer)?.nlink()
    );
    assert_ne!(payload_id, hardlink_id);
    assert_eq!(tree.record(payload_id).unwrap().identity, tree.record(hardlink_id).unwrap().identity);
    assert_eq!(tree.record(payload_id).unwrap().link_count, 2);
    assert_eq!(tree.record(hardlink_id).unwrap().link_count, 2);
    assert_eq!(tree.record(file_link_id).unwrap().entry_type, EntryType::Symlink);
    assert_eq!(tree.record(dangling_id).unwrap().entry_type, EntryType::Symlink);
    assert_eq!(tree.record(directory_link_id).unwrap().entry_type, EntryType::Symlink);
    assert_eq!(tree.children(directory_link_id).count(), 0);
    assert_eq!(tree.record(fifo_id).unwrap().entry_type, EntryType::Other);
    assert_eq!(tree.record(sparse_id).unwrap().apparent_bytes, fs::metadata(&sparse)?.len());
    assert_eq!(tree.record(sparse_id).unwrap().allocated_bytes, allocated(&sparse)?);

    let outer_apparent = [
        fs::symlink_metadata(&payload)?.len(),
        fs::symlink_metadata(&hardlink)?.len(),
        fs::symlink_metadata(&sparse)?.len(),
        fs::symlink_metadata(&unusual)?.len(),
        fs::symlink_metadata(&file_link)?.len(),
        fs::symlink_metadata(&dangling_link)?.len(),
        fs::symlink_metadata(&directory_link)?.len(),
        fs::symlink_metadata(&nested_payload)?.len(),
    ]
    .into_iter()
    .sum::<u64>();
    let outer_allocated = [
        allocated(&payload)?,
        allocated(&hardlink)?,
        allocated(&sparse)?,
        allocated(&unusual)?,
        allocated(&file_link)?,
        allocated(&dangling_link)?,
        allocated(&directory_link)?,
        allocated(&nested_payload)?,
    ]
    .into_iter()
    .sum::<u64>();
    assert_eq!(tree.record(outer_id).unwrap().apparent_bytes, outer_apparent);
    assert_eq!(tree.record(outer_id).unwrap().allocated_bytes, outer_allocated);
    assert_eq!(tree.record(root_file_id).unwrap().apparent_bytes, fs::metadata(&root_file)?.len());
    assert_eq!(tree.record(root_file_id).unwrap().allocated_bytes, allocated(&root_file)?);
    assert_eq!(
        tree.record(root_id).unwrap().apparent_bytes,
        outer_apparent + fs::metadata(root_file)?.len() + fs::metadata(&parallel_payload)?.len()
    );
    assert_eq!(
        tree.record(root_id).unwrap().allocated_bytes,
        outer_allocated + allocated(&actual.join("root-file"))? + allocated(&parallel_payload)?
    );

    for apparent in [false, true] {
        let summary = scan(&ScanOptions { path: supplied.clone(), apparent })?;
        assert_eq!(summary.mount_boundaries, 0);
        let outer_summary = summary
            .directories
            .iter()
            .find(|directory| directory.name.as_bytes() == b"outer")
            .expect("summary includes the immediate child directory");
        let indexed_size = if apparent {
            tree.record(outer_id).unwrap().apparent_bytes
        } else {
            tree.record(outer_id).unwrap().allocated_bytes
        };
        assert_eq!(outer_summary.disk_size, indexed_size);
        assert_eq!(outer_summary.exclusion, None);
    }
    Ok(())
}

#[test]
fn indexed_scan_batches_directory_finish_events_without_losing_states() -> io::Result<()> {
    let temp = TempDir::new()?;
    for index in 0..300 {
        fs::create_dir(temp.0.join(format!("directory-{index:03}")))?;
    }

    let result = scan_to_tree(open_root(&temp.0)?, 8)?;

    assert!(result.errors.is_empty(), "scan errors: {:?}", result.errors);
    assert!(result
        .directory_finish_batch_sizes
        .iter()
        .any(|size| *size > 1));
    assert_eq!(result.tree.record(result.tree.root()).unwrap().state, NodeState::Complete);
    assert!(result.tree.children(result.tree.root()).all(|node| {
        result.tree.record(node).unwrap().state == NodeState::Complete
    }));
    Ok(())
}

#[test]
fn indexed_worker_reopens_sibling_directories_after_the_handle_cap() -> io::Result<()> {
    let temp = TempDir::new()?;
    let wide = temp.0.join("wide");
    fs::create_dir(&wide)?;
    for index in 0..140 {
        let directory = wide.join(format!("branch-{index:03}"));
        fs::create_dir(&directory)?;
        fs::write(directory.join("payload"), b"x")?;
    }

    let result = scan_to_tree(open_root(&temp.0)?, 2)?;
    assert!(result.errors.is_empty(), "scanner errors: {:?}", result.errors);
    let wide_id = child_named(&result.tree, result.tree.root(), b"wide").unwrap();
    assert_eq!(result.tree.children(wide_id).count(), 140);
    for index in 0..140 {
        let name = format!("branch-{index:03}");
        let branch = child_named(&result.tree, wide_id, name.as_bytes()).unwrap();
        assert_eq!(result.tree.record(branch).unwrap().state, NodeState::Complete);
        assert_eq!(result.tree.children(branch).count(), 1);
    }
    Ok(())
}

#[test]
fn indexed_worker_publishes_wide_directory_chunks_before_completion() -> io::Result<()> {
    let temp = TempDir::new()?;
    for index in 0..2600 {
        fs::write(temp.0.join(format!("entry-{index:04}")), b"x")?;
    }
    let root = open_root(&temp.0)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let (batch_sender, batch_receiver) = mpsc::sync_channel(1);
    batch_sender
        .send(EntryBatch::with_capacity(DirectoryToken(0), BATCH_SIZE, BATCH_SIZE * 24))
        .unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = start_indexed_scan(root, sender, batch_receiver, Arc::clone(&cancelled));
    assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)), Ok(ScanEvent::Started { .. })));
    let first = receiver.recv_timeout(Duration::from_secs(5)).map_err(io::Error::other)?;
    match first {
        ScanEvent::Entries(batch) => assert_eq!(batch.entries.len(), BATCH_SIZE),
        other => panic!("expected entry batch, got {other:?}"),
    }
    let second = receiver.recv_timeout(Duration::from_secs(5)).map_err(io::Error::other)?;
    assert!(matches!(second, ScanEvent::Entries(_)));
    cancelled.store(true, Ordering::Relaxed);
    drop(receiver);
    worker.join().map_err(|_| io::Error::new(io::ErrorKind::Other, "scanner worker panicked"))?;
    Ok(())
}

#[test]
fn dropping_a_full_scan_queue_releases_a_blocked_worker() -> io::Result<()> {
    let temp = TempDir::new()?;
    for index in 0..1000 {
        fs::write(temp.0.join(format!("entry-{index:04}")), b"x")?;
    }
    let root = open_root(&temp.0)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let (batch_sender, batch_receiver) = mpsc::sync_channel(1);
    batch_sender
        .send(EntryBatch::with_capacity(DirectoryToken(0), BATCH_SIZE, BATCH_SIZE * 24))
        .unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = start_indexed_scan(root, sender, batch_receiver, Arc::clone(&cancelled));
    assert!(matches!(receiver.recv_timeout(Duration::from_secs(5)), Ok(ScanEvent::Started { .. })));
    cancelled.store(true, Ordering::Relaxed);
    drop(receiver);
    worker.join().map_err(|_| io::Error::new(io::ErrorKind::Other, "scanner worker panicked"))?;
    Ok(())
}

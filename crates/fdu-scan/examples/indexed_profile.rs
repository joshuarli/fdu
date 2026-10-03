use fdu_core::{DirectoryToken, EntryBatch, EntryType, NodeState, ScanEvent, Tree};
use fdu_scan::{open_root, start_indexed_scan_with_metrics, ScanQueueMetrics};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

const CHANNEL_CAPACITY: usize = 8;
const BATCH_CAPACITY: usize = 256;

fn main() -> io::Result<()> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "usage: indexed_profile PATH")
    })?;
    if std::env::args_os().nth(2).is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "indexed_profile accepts one root path"));
    }

    let started = Instant::now();
    let path = PathBuf::from(path);
    let root = open_root(&path)?;
    let mut tree = Tree::new(&root.name, root.identity, root.link_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root name exceeds index limits"))?;
    let mut token_nodes = vec![Some(tree.root())];
    let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let (batch_sender, batch_receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
    for _ in 0..CHANNEL_CAPACITY {
        batch_sender
            .send(EntryBatch::with_capacity(
                DirectoryToken(0),
                BATCH_CAPACITY,
                BATCH_CAPACITY * 24,
            ))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "batch pool closed"))?;
    }
    let metrics = ScanQueueMetrics::new(CHANNEL_CAPACITY, true);
    let worker = start_indexed_scan_with_metrics(
        root,
        sender,
        batch_receiver,
        Arc::new(AtomicBool::new(false)),
        metrics.clone(),
    );
    let mut first_batch = None;
    let mut scan_errors = 0usize;
    let mut done = false;

    while !done {
        let event = receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "scanner event channel closed"))?;
        metrics.event_received();
        match event {
            ScanEvent::Started { identity, .. } => {
                if identity != tree.record(tree.root()).expect("root exists").identity {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "scan root identity changed"));
                }
            }
            ScanEvent::Entries(mut batch) => {
                first_batch.get_or_insert_with(|| started.elapsed());
                let parent = token_nodes
                    .get(batch.directory.0 as usize)
                    .copied()
                    .flatten()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown directory token"))?;
                let mut apparent_batch = 0u64;
                let mut allocated_batch = 0u64;
                let mut totals_fit = true;
                for entry in &batch.entries {
                    let name = batch
                        .names
                        .get(entry.name.start as usize..entry.name.end as usize)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid name range"))?;
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
                        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "index capacity exceeded"))?;
                    if let Some(token) = entry.directory_token {
                        let token_index = token.0 as usize;
                        if token_nodes.len() <= token_index {
                            token_nodes.resize(token_index + 1, None);
                        }
                        token_nodes[token_index] = Some(id);
                    }
                    if entry.entry_type != EntryType::Directory && entry.state == NodeState::Complete {
                        match (
                            apparent_batch.checked_add(entry.apparent_bytes),
                            allocated_batch.checked_add(entry.allocated_bytes),
                        ) {
                            (Some(apparent), Some(allocated)) => {
                                apparent_batch = apparent;
                                allocated_batch = allocated;
                            }
                            _ => {
                                totals_fit = false;
                                tree.mark_incomplete_to_root(parent);
                                scan_errors = scan_errors.saturating_add(1);
                            }
                        }
                    } else if entry.state == NodeState::Incomplete {
                        tree.mark_incomplete_to_root(parent);
                        scan_errors = scan_errors.saturating_add(1);
                    }
                }
                if totals_fit && !tree.add_to_ancestors(parent, apparent_batch, allocated_batch) {
                    tree.mark_incomplete_to_root(parent);
                    scan_errors = scan_errors.saturating_add(1);
                }
                batch.clear_for_directory(DirectoryToken(0));
                let _ = batch_sender.try_send(batch);
            }
            ScanEvent::DirectoryFinished { directory, complete } => {
                if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                    if complete && tree.record(node).is_some_and(|record| record.state == NodeState::Scanning) {
                        tree.set_state(node, NodeState::Complete);
                    } else if !complete {
                        tree.mark_incomplete_to_root(node);
                    }
                }
            }
            ScanEvent::DirectoriesFinished(directories) => {
                for (directory, complete) in directories {
                    if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                        if complete && tree.record(node).is_some_and(|record| record.state == NodeState::Scanning) {
                            tree.set_state(node, NodeState::Complete);
                        } else if !complete {
                            tree.mark_incomplete_to_root(node);
                        }
                    }
                }
            }
            ScanEvent::DirectoryExcluded { directory, reason } => {
                if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                    tree.set_state(node, NodeState::Excluded(reason));
                }
            }
            ScanEvent::DirectoryFailed { directory, .. } => {
                scan_errors = scan_errors.saturating_add(1);
                if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                    tree.mark_incomplete_to_root(node);
                }
            }
            ScanEvent::Failed { .. } | ScanEvent::Cancelled => {
                scan_errors = scan_errors.saturating_add(1);
                tree.mark_incomplete_to_root(tree.root());
            }
            ScanEvent::Finished => done = true,
        }
    }
    worker
        .join()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "scanner worker panicked"))?;

    let entries = tree.len().saturating_sub(1);
    let arena_bytes = tree.retained_arena_bytes();
    eprintln!(
        "fdu-indexed-profile elapsed_ms={} first_batch_ms={} entries={} arena_bytes={} arena_bytes_per_entry={:.2} event_queue_high_water={} scan_errors={}",
        started.elapsed().as_millis(),
        first_batch.map_or(0, |duration| duration.as_millis()),
        entries,
        arena_bytes,
        if entries == 0 { 0.0 } else { arena_bytes as f64 / entries as f64 },
        metrics.event_queue_high_water(),
        scan_errors,
    );
    Ok(())
}

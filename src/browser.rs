use fdu_core::{
    DirectoryToken, EntryType, ExclusionReason, NodeId, NodeState, ScanEvent, Tree,
};
use fdu_delete::{create_plan, DeleteEvent, DeleteOutcome, DeletionPlan, OutcomeKind};
use fdu_scan::{open_root, start_indexed_scan_with_metrics, RootAnchor, ScanQueueMetrics};
use fdu_tui::{self, Cursor, Intent, Modal, Phase, SizeMode, SortMode, TerminalSession, View};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SCAN_EVENT_CHANNEL_CAPACITY: usize = 512;
const SCAN_BATCH_CAPACITY: usize = 1024;
const SCAN_BATCH_POOL_CAPACITY: usize = 8;
const DELETE_CHANNEL_CAPACITY: usize = 8;
const MAX_SCAN_EVENTS_PER_TICK: usize = 128;
const MAX_DELETE_EVENTS_PER_TICK: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SCAN_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const SCAN_REDRAW_INTERVAL: Duration = Duration::from_millis(100);

enum AppPhase {
    Scanning(ScanRun),
    Ready,
    Deleting(DeleteRun),
}

impl AppPhase {
    fn view_phase(&self) -> Phase {
        match self {
            Self::Scanning(_) => Phase::Scanning,
            Self::Ready => Phase::Ready,
            Self::Deleting(_) => Phase::Deleting,
        }
    }
}

struct ScanRun {
    receiver: Option<Receiver<ScanEvent>>,
    returned_batches: Option<SyncSender<fdu_core::EntryBatch>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    queue_metrics: ScanQueueMetrics,
}

impl ScanRun {
    fn start(root: RootAnchor, queue_metrics: ScanQueueMetrics) -> Self {
        let (sender, receiver) = mpsc::sync_channel(SCAN_EVENT_CHANNEL_CAPACITY);
        let (batch_sender, batch_receiver) = mpsc::sync_channel(SCAN_BATCH_POOL_CAPACITY);
        for _ in 0..SCAN_BATCH_POOL_CAPACITY {
            let batch = fdu_core::EntryBatch::with_capacity(
                DirectoryToken(0),
                SCAN_BATCH_CAPACITY,
                SCAN_BATCH_CAPACITY * 24,
            );
            if batch_sender.send(batch).is_err() {
                break;
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = start_indexed_scan_with_metrics(
            root,
            sender,
            batch_receiver,
            Arc::clone(&cancelled),
            queue_metrics.clone(),
        );
        Self {
            receiver: Some(receiver),
            returned_batches: Some(batch_sender),
            cancelled,
            worker: Some(worker),
            queue_metrics,
        }
    }

    fn return_batch(&self, mut batch: fdu_core::EntryBatch) {
        batch.clear_for_directory(DirectoryToken(0));
        if let Some(sender) = &self.returned_batches {
            let _ = sender.try_send(batch);
        }
    }

    fn finish(&mut self) -> Result<(), String> {
        let worker_result = self.worker.take().map(JoinHandle::join);
        self.receiver.take();
        self.returned_batches.take();
        match worker_result {
            Some(Ok(())) => Ok(()),
            Some(Err(_)) => Err("scanner worker terminated unexpectedly".to_owned()),
            None => Ok(()),
        }
    }
}

impl Drop for ScanRun {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.receiver.take();
        self.returned_batches.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct DeleteRun {
    receiver: Option<Receiver<DeleteEvent>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    outcomes: Vec<DeleteOutcome>,
    failure: Option<String>,
    completed: usize,
    total: usize,
    current: String,
}

impl DeleteRun {
    fn start(root: std::os::fd::OwnedFd, plan: DeletionPlan) -> Self {
        let total = plan.operation_count();
        let (sender, receiver) = mpsc::sync_channel(DELETE_CHANNEL_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            fdu_delete::execute(root, plan, worker_cancelled, sender);
        });
        Self {
            receiver: Some(receiver),
            cancelled,
            worker: Some(worker),
            outcomes: Vec::with_capacity(total.min(256)),
            failure: None,
            completed: 0,
            total,
            current: String::new(),
        }
    }

    fn finish(&mut self) -> Result<(Vec<DeleteOutcome>, Option<String>), String> {
        let worker_result = self.worker.take().map(JoinHandle::join);
        self.receiver.take();
        match worker_result {
            Some(Ok(())) => Ok((std::mem::take(&mut self.outcomes), self.failure.take())),
            Some(Err(_)) => Err("deletion worker terminated unexpectedly".to_owned()),
            None => Ok((std::mem::take(&mut self.outcomes), self.failure.take())),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Drop for DeleteRun {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum DialogState {
    None,
    Help,
    Filter(String),
    Confirm {
        plan: Option<DeletionPlan>,
        selected: Vec<NodeId>,
        rejected: Vec<String>,
    },
}

enum Action {
    None,
    Quit,
    Refresh,
    Delete(DeletionPlan),
    CancelDeletion,
}

struct BrowserModel {
    tree: Arc<Tree>,
    token_nodes: Vec<Option<NodeId>>,
    current_directory: NodeId,
    listing: Vec<NodeId>,
    visible: Vec<NodeId>,
    cursor: Cursor,
    cursor_index: Option<usize>,
    cursor_moved_during_scan: bool,
    marks: HashSet<NodeId>,
    filter: String,
    size_mode: SizeMode,
    sort_mode: SortMode,
    range_mode: bool,
    range_anchor: Option<NodeId>,
    dialog: DialogState,
    message: Option<String>,
    status_by_node: HashMap<NodeId, String>,
    indexed_entries: usize,
    incomplete_entries: usize,
    exclusions: usize,
    scan_errors: usize,
    actual_deletions: usize,
    scanning: bool,
    read_only: bool,
}

struct BrowserProfile {
    enabled: bool,
    started: Instant,
    first_render: Option<Duration>,
    first_usable_listing: Option<Duration>,
    initial_scan_settled: Option<Duration>,
    pending_input: Option<Instant>,
    input_render_micros: Vec<u64>,
    queue_metrics: ScanQueueMetrics,
}

impl BrowserProfile {
    fn new(enabled: bool, queue_metrics: ScanQueueMetrics) -> Self {
        Self {
            enabled,
            started: Instant::now(),
            first_render: None,
            first_usable_listing: None,
            initial_scan_settled: None,
            pending_input: None,
            input_render_micros: Vec::new(),
            queue_metrics,
        }
    }

    fn reset_scan_queue(&mut self, queue_metrics: ScanQueueMetrics) {
        self.queue_metrics = queue_metrics;
    }

    fn note_input(&mut self) {
        if self.enabled {
            self.pending_input = Some(Instant::now());
        }
    }

    fn note_render(&mut self, model: &BrowserModel, phase: Phase) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started.elapsed();
        self.first_render.get_or_insert(elapsed);
        let root_is_visible = model.current_directory == model.tree.root() && !model.visible.is_empty();
        if self.first_usable_listing.is_none() && (root_is_visible || phase == Phase::Ready) {
            self.first_usable_listing = Some(elapsed);
        }
        if let Some(input) = self.pending_input.take() {
            self.input_render_micros
                .push(input.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
        }
    }

    fn note_scan_settled(&mut self) {
        self.initial_scan_settled.get_or_insert_with(|| self.started.elapsed());
    }

    fn report(&mut self, model: &BrowserModel) {
        if !self.enabled {
            return;
        }
        self.input_render_micros.sort_unstable();
        let median = self
            .input_render_micros
            .get(self.input_render_micros.len() / 2)
            .copied();
        let maximum = self.input_render_micros.last().copied();
        let entries = model.indexed_entries;
        let arena_bytes = model.tree.retained_arena_bytes();
        eprintln!(
            "fdu-profile elapsed_ms={} initial_scan_settled_ms={} first_render_ms={} first_usable_listing_ms={} entries={} tree_arena_bytes={} tree_arena_bytes_per_entry={:.2} scan_event_queue_high_water={} input_render_samples={} input_render_p50_us={} input_render_max_us={}",
            self.started.elapsed().as_millis(),
            duration_ms(self.initial_scan_settled),
            duration_ms(self.first_render),
            duration_ms(self.first_usable_listing),
            entries,
            arena_bytes,
            if entries == 0 { 0.0 } else { arena_bytes as f64 / entries as f64 },
            self.queue_metrics.event_queue_high_water(),
            self.input_render_micros.len(),
            median.unwrap_or(0),
            maximum.unwrap_or(0),
        );
    }
}

fn duration_ms(duration: Option<Duration>) -> u128 {
    duration.map_or(0, |duration| duration.as_millis())
}

impl BrowserModel {
    fn new(root: &RootAnchor, read_only: bool, apparent: bool) -> io::Result<Self> {
        let tree = Tree::new(&root.name, root.identity, root.link_count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "root path exceeds index limits")
        })?;
        let current_directory = tree.root();
        let mut model = Self {
            tree: Arc::new(tree),
            token_nodes: vec![Some(current_directory)],
            current_directory,
            listing: Vec::new(),
            visible: Vec::new(),
            cursor: Cursor::None,
            cursor_index: None,
            cursor_moved_during_scan: false,
            marks: HashSet::new(),
            filter: String::new(),
            size_mode: SizeMode::Allocated,
            sort_mode: SortMode::Size,
            range_mode: false,
            range_anchor: None,
            dialog: DialogState::None,
            message: None,
            status_by_node: HashMap::new(),
            indexed_entries: 0,
            incomplete_entries: 0,
            exclusions: 0,
            scan_errors: 0,
            actual_deletions: 0,
            scanning: true,
            read_only,
        };
        model.size_mode = if apparent { SizeMode::Apparent } else { SizeMode::Allocated };
        model.rebuild_listing(false);
        Ok(model)
    }

    fn reset_for_rescan(&mut self, root: &RootAnchor) -> io::Result<()> {
        let tree = Tree::new(&root.name, root.identity, root.link_count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "root path exceeds index limits")
        })?;
        self.current_directory = tree.root();
        self.tree = Arc::new(tree);
        self.token_nodes.clear();
        self.token_nodes.push(Some(self.current_directory));
        self.listing.clear();
        self.visible.clear();
        self.cursor = Cursor::None;
        self.cursor_index = None;
        self.cursor_moved_during_scan = false;
        self.marks.clear();
        self.filter.clear();
        self.range_mode = false;
        self.range_anchor = None;
        self.dialog = DialogState::None;
        self.message = None;
        self.status_by_node.clear();
        self.indexed_entries = 0;
        self.incomplete_entries = 0;
        self.exclusions = 0;
        self.scan_errors = 0;
        self.scanning = true;
        self.rebuild_listing(false);
        Ok(())
    }

    fn apply_scan_event(&mut self, event: ScanEvent) -> bool {
        match event {
            ScanEvent::Started {
                identity,
                link_count: _,
                root_name: _,
            } => {
                let root = self.tree.record(self.tree.root()).expect("root record exists");
                if root.identity != identity {
                    self.message = Some("Opened root identity changed before scanning started.".to_owned());
                    self.set_state(self.tree.root(), NodeState::Incomplete);
                    self.mark_incomplete_chain(self.tree.root());
                }
                true
            }
            ScanEvent::Entries(_) => false,
            ScanEvent::DirectoryFinished { directory, complete } => {
                self.finish_directory(directory, complete);
                true
            }
            ScanEvent::DirectoriesFinished(directories) => {
                for (directory, complete) in directories {
                    self.finish_directory(directory, complete);
                }
                true
            }
            ScanEvent::DirectoryExcluded { directory, reason } => {
                if let Some(node) = self.node_for_token(directory) {
                    self.set_state(node, NodeState::Excluded(reason));
                    if let Some(parent) = self.tree.record(node).and_then(|record| record.parent()) {
                        self.mark_incomplete_chain(parent);
                    }
                }
                true
            }
            ScanEvent::DirectoryFailed { directory, message } => {
                self.scan_errors = self.scan_errors.saturating_add(1);
                if self.message.is_none() {
                    self.message = Some(format!("Scan error: {message}"));
                }
                if let Some(node) = self.node_for_token(directory) {
                    self.mark_incomplete_chain(node);
                }
                true
            }
            ScanEvent::Failed { message } => {
                self.scan_errors = self.scan_errors.saturating_add(1);
                self.message = Some(format!("Scan could not continue: {message}"));
                self.mark_incomplete_chain(self.tree.root());
                true
            }
            ScanEvent::Cancelled => {
                self.cancel_incomplete_nodes();
                self.message = Some("Scan cancelled; visible totals may be incomplete.".to_owned());
                true
            }
            ScanEvent::Finished => true,
        }
    }

    fn finish_directory(&mut self, directory: DirectoryToken, complete: bool) {
        if let Some(node) = self.node_for_token(directory) {
            let old = self.tree.record(node).map(|record| record.state);
            if old.is_some_and(|state| !matches!(state, NodeState::Excluded(_) | NodeState::Tombstone)) {
                if complete {
                    if old == Some(NodeState::Scanning) {
                        self.set_state(node, NodeState::Complete);
                    }
                } else {
                    self.mark_incomplete_chain(node);
                }
            }
        }
    }

    fn apply_entry_batch(&mut self, mut batch: fdu_core::EntryBatch) -> fdu_core::EntryBatch {
        let Some(parent) = self.node_for_token(batch.directory) else {
            self.message = Some("Scanner sent an entry batch for an unknown directory.".to_owned());
            self.mark_incomplete_chain(self.tree.root());
            return batch;
        };
        let mut new_ids = Vec::with_capacity(batch.entries.len());
        let mut apparent_batch = 0u64;
        let mut allocated_batch = 0u64;
        let mut totals_fit = true;
        for entry in &batch.entries {
            let Some(name) = batch.names.get(entry.name.start as usize..entry.name.end as usize) else {
                self.message = Some("Scanner sent an invalid filename range.".to_owned());
                self.mark_incomplete_chain(parent);
                continue;
            };
            let Some(tree) = Arc::get_mut(&mut self.tree) else {
                self.message = Some("Index became shared while scanning was active.".to_owned());
                totals_fit = false;
                break;
            };
            let id = match tree.append(
                parent,
                name,
                entry.entry_type,
                entry.identity,
                entry.link_count,
                entry.apparent_bytes,
                entry.allocated_bytes,
                entry.state,
            ) {
                Some(id) => id,
                None => {
                    self.message = Some("Index exhausted the supported number of entries.".to_owned());
                    self.mark_incomplete_chain(parent);
                    break;
                }
            };
            new_ids.push(id);
            if let Some(token) = entry.directory_token {
                let index = token.0 as usize;
                if self.token_nodes.len() <= index {
                    self.token_nodes.resize(index + 1, None);
                }
                self.token_nodes[index] = Some(id);
            }
            match entry.state {
                NodeState::Incomplete => {
                    self.bump_incomplete(1);
                    self.mark_incomplete_chain(parent);
                }
                NodeState::Excluded(reason) => {
                    self.bump_exclusion(reason, 1);
                    self.mark_incomplete_chain(parent);
                }
                _ => {}
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
                    _ => totals_fit = false,
                }
            }
        }
        self.indexed_entries = self.tree.len().saturating_sub(1);
        if totals_fit {
            if !self.add_batch_totals(parent, apparent_batch, allocated_batch) {
                self.message = Some("A directory total exceeded the supported byte range.".to_owned());
                self.mark_incomplete_chain(parent);
            }
        } else {
            self.message = Some("A directory total exceeded the supported byte range.".to_owned());
            self.mark_incomplete_chain(parent);
        }
        if parent == self.current_directory {
            self.listing.extend(new_ids.iter().copied());
            for id in new_ids {
                if self.filter_matches(id) {
                    self.visible.push(id);
                }
            }
            if matches!(self.cursor, Cursor::None) {
                self.cursor = self.visible.first().copied().map(Cursor::Entry).unwrap_or_else(|| {
                    if self.has_parent_row() { Cursor::Parent } else { Cursor::None }
                });
                self.refresh_cursor_index();
            }
        }
        batch.entries.clear();
        batch.names.clear();
        batch
    }

    fn add_batch_totals(&mut self, parent: NodeId, apparent: u64, allocated: u64) -> bool {
        let mut current = Some(parent);
        while let Some(node) = current {
            let Some(record) = self.tree.record(node) else {
                return false;
            };
            if record.apparent_bytes.checked_add(apparent).is_none()
                || record.allocated_bytes.checked_add(allocated).is_none()
            {
                return false;
            }
            current = record.parent();
        }
        let Some(tree) = Arc::get_mut(&mut self.tree) else {
            return false;
        };
        if !tree.add_to_ancestors(parent, apparent, allocated) {
            return false;
        }
        true
    }

    fn handle_intent(&mut self, intent: Intent, phase: Phase) -> (Action, bool) {
        if matches!(&self.dialog, DialogState::Help) {
            if matches!(intent, Intent::Cancel | Intent::Help) {
                self.dialog = DialogState::None;
                return (Action::None, true);
            }
            return (Action::None, false);
        }
        if let DialogState::Filter(value) = &mut self.dialog {
            match intent {
                Intent::FilterCharacter(character) => {
                    value.push(character);
                    return (Action::None, true);
                }
                Intent::FilterBackspace => {
                    value.pop();
                    return (Action::None, true);
                }
                Intent::SubmitFilter => {
                    let DialogState::Filter(value) = std::mem::replace(&mut self.dialog, DialogState::None) else {
                        unreachable!()
                    };
                    self.filter = value;
                    self.range_mode = false;
                    self.range_anchor = None;
                    self.rebuild_visible(true);
                    self.message = None;
                    return (Action::None, true);
                }
                Intent::Cancel => {
                    self.dialog = DialogState::None;
                    return (Action::None, true);
                }
                _ => return (Action::None, false),
            }
        }
        if matches!(&self.dialog, DialogState::Confirm { .. }) {
            match intent {
                Intent::Cancel => {
                    self.dialog = DialogState::None;
                    return (Action::None, true);
                }
                Intent::Enter => {
                    let DialogState::Confirm { plan, .. } = std::mem::replace(&mut self.dialog, DialogState::None) else {
                        unreachable!()
                    };
                    if let Some(plan) = plan {
                        return (Action::Delete(plan), true);
                    }
                    self.dialog = DialogState::None;
                    return (Action::None, true);
                }
                _ => return (Action::None, false),
            }
        }
        if phase == Phase::Deleting {
            return if matches!(intent, Intent::Cancel) {
                (Action::CancelDeletion, true)
            } else {
                (Action::None, false)
            };
        }
        if matches!(intent, Intent::Help) {
            self.dialog = DialogState::Help;
            return (Action::None, true);
        }
        if matches!(intent, Intent::Quit) {
            if phase == Phase::Scanning {
                self.message = Some("Scan cancelled on exit.".to_owned());
            }
            return (Action::Quit, true);
        }
        if matches!(intent, Intent::StartFilter) {
            self.dialog = DialogState::Filter(self.filter.clone());
            self.message = None;
            return (Action::None, true);
        }
        if phase == Phase::Scanning {
            match intent {
                Intent::Delete => {
                    self.message = Some("Deletion is unavailable until scanning finishes.".to_owned());
                    return (Action::None, true);
                }
                Intent::Refresh => {
                    self.message = Some("A full rescan is unavailable while scanning.".to_owned());
                    return (Action::None, true);
                }
                _ => {}
            }
        }
        match intent {
            Intent::MoveUp => (Action::None, self.move_cursor(-1)),
            Intent::MoveDown => (Action::None, self.move_cursor(1)),
            Intent::PageUp => (Action::None, self.move_cursor(-15)),
            Intent::PageDown => (Action::None, self.move_cursor(15)),
            Intent::Home => (Action::None, self.move_to_edge(false)),
            Intent::End => (Action::None, self.move_to_edge(true)),
            Intent::Enter => (Action::None, self.enter_cursor()),
            Intent::Parent => (Action::None, self.go_parent()),
            Intent::ToggleMark => (Action::None, self.toggle_mark()),
            Intent::ToggleRange => (Action::None, self.toggle_range()),
            Intent::MarkAll => (Action::None, self.mark_all_visible()),
            Intent::ClearMarks => {
                let changed = !self.marks.is_empty();
                self.marks.clear();
                (Action::None, changed)
            }
            Intent::ToggleSizeMode => {
                self.size_mode = match self.size_mode {
                    SizeMode::Allocated => SizeMode::Apparent,
                    SizeMode::Apparent => SizeMode::Allocated,
                };
                let changed = if phase == Phase::Ready { self.sort_visible_if_needed() } else { true };
                (Action::None, changed)
            }
            Intent::SortByName => (Action::None, self.set_sort(SortMode::Name, phase)),
            Intent::SortBySize => (Action::None, self.set_sort(SortMode::Size, phase)),
            Intent::Delete => self.begin_delete(),
            Intent::Refresh if phase == Phase::Ready => (Action::Refresh, true),
            Intent::Refresh => {
                self.message = Some("A full rescan is only available when the index is ready.".to_owned());
                (Action::None, true)
            }
            Intent::Cancel => (Action::None, false),
            Intent::FilterCharacter(_)
            | Intent::FilterBackspace
            | Intent::SubmitFilter
            | Intent::StartFilter
            | Intent::Help
            | Intent::Quit => (Action::None, false),
        }
    }

    fn begin_delete(&mut self) -> (Action, bool) {
        if self.read_only {
            self.message = Some("This session is read only; deletion is disabled.".to_owned());
            return (Action::None, true);
        }
        self.range_mode = false;
        self.range_anchor = None;
        let selected = if self.marks.is_empty() {
            match self.cursor {
                Cursor::Entry(id) => vec![id],
                _ => Vec::new(),
            }
        } else {
            self.listing
                .iter()
                .copied()
                .filter(|id| self.marks.contains(id))
                .collect()
        };
        match create_plan(Arc::clone(&self.tree), &selected) {
            Ok(plan) => {
                self.dialog = DialogState::Confirm {
                    plan: Some(plan),
                    selected,
                    rejected: Vec::new(),
                };
                self.message = None;
            }
            Err(error) => {
                self.dialog = DialogState::Confirm {
                    plan: None,
                    selected,
                    rejected: error.rejected,
                };
                self.message = Some("The full selection is ineligible; no entries were narrowed or removed.".to_owned());
            }
        }
        (Action::None, true)
    }

    fn view<'a>(&'a self, phase: &'a AppPhase) -> View<'a> {
        let modal = match &self.dialog {
            DialogState::None => match phase {
                AppPhase::Deleting(worker) => Modal::Deleting {
                    attempted: worker.completed,
                    total: worker.total,
                    current: &worker.current,
                    cancellable: !worker.cancelled.load(Ordering::Relaxed),
                },
                _ => Modal::None,
            },
            DialogState::Help => Modal::Help,
            DialogState::Filter(value) => Modal::Filter { value },
            DialogState::Confirm {
                plan,
                selected,
                rejected,
            } => match plan {
                Some(plan) => Modal::ConfirmDelete {
                    roots: &plan.roots,
                    descendants: plan.operation_count(),
                    apparent_bytes: plan.apparent_bytes,
                    allocated_bytes: plan.allocated_bytes,
                    rejected,
                    eligible: true,
                },
                None => Modal::ConfirmDelete {
                    roots: selected,
                    descendants: 0,
                    apparent_bytes: 0,
                    allocated_bytes: 0,
                    rejected,
                    eligible: false,
                },
            },
        };
        let detail_message = match self.cursor {
            Cursor::Entry(id) => self.status_by_node.get(&id).map(String::as_str),
            _ => None,
        };
        View {
            tree: &self.tree,
            current_directory: self.current_directory,
            rows: &self.visible,
            has_parent_row: self.has_parent_row(),
            cursor_index: self.cursor_index,
            marks: &self.marks,
            filter: &self.filter,
            size_mode: self.size_mode,
            sort_mode: self.sort_mode,
            phase: phase.view_phase(),
            indexed_entries: self.indexed_entries,
            directory_items: self.listing.len(),
            visible_items: self.visible.len(),
            incomplete_entries: self.incomplete_entries,
            exclusions: self.exclusions,
            read_only: self.read_only,
            range_mode: self.range_mode,
            message: self.message.as_deref(),
            detail_message,
            modal,
        }
    }

    fn finish_scan(&mut self, error: Option<String>) {
        let failed = error.is_some();
        if let Some(error) = error {
            self.message = Some(error);
            self.scan_errors = self.scan_errors.saturating_add(1);
            self.mark_incomplete_chain(self.tree.root());
        }
        if failed {
            let changed = Arc::get_mut(&mut self.tree)
                .expect("scan worker is joined before model updates")
                .mark_scanning_incomplete();
            self.bump_incomplete(changed);
        } else if self.tree.record(self.tree.root()).is_some_and(|record| record.state == NodeState::Scanning) {
            self.set_state(self.tree.root(), NodeState::Complete);
        }
        self.range_mode = false;
        self.range_anchor = None;
        self.scanning = false;
        let preserve_cursor = self.cursor_moved_during_scan;
        self.cursor_moved_during_scan = false;
        self.rebuild_listing(preserve_cursor);
        if self.message.is_none() {
            self.message = Some(format!("Index ready · {} entries · {} scan errors", self.indexed_entries, self.scan_errors));
        }
    }

    fn apply_delete_outcomes(&mut self, outcomes: Vec<DeleteOutcome>, worker_error: Option<String>) {
        let mut deleted = 0usize;
        let mut absent = 0usize;
        let mut failures = 0usize;
        let mut cancelled = 0usize;
        for outcome in outcomes {
            match outcome.kind {
                OutcomeKind::Deleted => {
                    deleted = deleted.saturating_add(1);
                    if self.tombstone(outcome.node).is_none() {
                        failures = failures.saturating_add(1);
                        self.mark_stale_chain(outcome.node);
                        self.status_by_node.insert(outcome.node, "Deleted on disk; index accounting could not be reconciled. Refresh required.".to_owned());
                    }
                }
                OutcomeKind::AlreadyAbsent => {
                    absent = absent.saturating_add(1);
                    if self.tombstone(outcome.node).is_none() {
                        failures = failures.saturating_add(1);
                        self.mark_stale_chain(outcome.node);
                    }
                }
                OutcomeKind::Changed | OutcomeKind::Failed => {
                    failures = failures.saturating_add(1);
                    let message = outcome.message.unwrap_or_else(|| "entry could not be removed".to_owned());
                    self.mark_stale_chain(outcome.node);
                    self.status_by_node.insert(outcome.node, message);
                }
                OutcomeKind::Cancelled => cancelled = cancelled.saturating_add(1),
            }
        }
        if let Some(error) = worker_error {
            self.message = Some(error);
            self.mark_stale_chain(self.tree.root());
        } else {
            self.message = Some(format!(
                "Deleted {deleted} · {absent} already absent · {failures} changed or failed · {cancelled} not attempted"
            ));
        }
        self.actual_deletions = self.actual_deletions.saturating_add(deleted);
        self.marks.clear();
        self.range_mode = false;
        self.range_anchor = None;
        self.rebuild_listing(true);
    }

    fn node_for_token(&self, token: DirectoryToken) -> Option<NodeId> {
        self.token_nodes.get(token.0 as usize).copied().flatten()
    }

    fn set_state(&mut self, id: NodeId, state: NodeState) {
        let old = self.tree.record(id).map(|record| record.state);
        if old == Some(state) {
            return;
        }
        if let Some(old) = old {
            self.adjust_state_count(old, -1);
        }
        if Arc::get_mut(&mut self.tree).is_some_and(|tree| tree.set_state(id, state)) {
            self.adjust_state_count(state, 1);
        }
    }

    fn adjust_state_count(&mut self, state: NodeState, amount: isize) {
        match state {
            NodeState::Incomplete => {
                if amount > 0 {
                    self.incomplete_entries = self.incomplete_entries.saturating_add(amount as usize);
                } else {
                    self.incomplete_entries = self.incomplete_entries.saturating_sub(amount.unsigned_abs());
                }
            }
            NodeState::Excluded(reason) => self.bump_exclusion(reason, amount),
            _ => {}
        }
    }

    fn bump_incomplete(&mut self, count: usize) {
        self.incomplete_entries = self.incomplete_entries.saturating_add(count);
    }

    fn bump_exclusion(&mut self, reason: ExclusionReason, amount: isize) {
        let counter = &mut self.exclusions;
        match reason {
            ExclusionReason::MountBoundary | ExclusionReason::UnsupportedAlias => {
                if amount > 0 {
                    *counter = counter.saturating_add(amount as usize);
                } else {
                    *counter = counter.saturating_sub(amount.unsigned_abs());
                }
            }
        }
    }

    fn mark_incomplete_chain(&mut self, start: NodeId) {
        let mut current = Some(start);
        while let Some(id) = current {
            let Some(record) = self.tree.record(id) else {
                break;
            };
            let parent = record.parent();
            if matches!(record.state, NodeState::Scanning | NodeState::Complete) {
                self.set_state(id, NodeState::Incomplete);
            }
            current = parent;
        }
    }

    fn mark_stale_chain(&mut self, start: NodeId) {
        let mut current = Some(start);
        while let Some(id) = current {
            let Some(record) = self.tree.record(id) else {
                break;
            };
            let parent = record.parent();
            if record.state != NodeState::Tombstone {
                self.set_state(id, NodeState::Stale);
            }
            current = parent;
        }
    }

    fn cancel_incomplete_nodes(&mut self) {
        let changed = Arc::get_mut(&mut self.tree)
            .expect("scanner is joined before cancellation is applied")
            .mark_scanning_incomplete();
        self.bump_incomplete(changed);
    }

    fn tombstone(&mut self, id: NodeId) -> Option<(u64, u64)> {
        let (apparent, allocated, removed_entries) =
            Arc::get_mut(&mut self.tree)?.tombstone_subtree(id)?;
        self.indexed_entries = self.indexed_entries.saturating_sub(removed_entries);
        Some((apparent, allocated))
    }

    fn rebuild_listing(&mut self, preserve_cursor: bool) {
        let previous = if preserve_cursor { self.cursor } else { Cursor::None };
        self.listing = self.tree.children(self.current_directory).collect();
        if !self.is_scanning() {
            self.sort_listing();
        }
        self.visible = self
            .listing
            .iter()
            .copied()
            .filter(|id| self.filter_matches(*id))
            .collect();
        self.cursor = self.valid_cursor(previous).unwrap_or_else(|| self.first_cursor());
        self.refresh_cursor_index();
    }

    fn rebuild_visible(&mut self, preserve_cursor: bool) {
        let previous = if preserve_cursor { self.cursor } else { Cursor::None };
        self.visible = self
            .listing
            .iter()
            .copied()
            .filter(|id| self.filter_matches(*id))
            .collect();
        self.cursor = self.valid_cursor(previous).unwrap_or_else(|| self.first_cursor());
        self.refresh_cursor_index();
    }

    fn sort_listing(&mut self) {
        let mode = self.sort_mode;
        let size_mode = self.size_mode;
        let tree = &self.tree;
        self.listing.sort_unstable_by(|left, right| {
            let left_record = tree.record(*left).expect("listing node exists");
            let right_record = tree.record(*right).expect("listing node exists");
            match mode {
                SortMode::Name => tree.name(*left).unwrap_or_default().cmp(tree.name(*right).unwrap_or_default()),
                SortMode::Size => {
                    let left_complete = left_record.state == NodeState::Complete;
                    let right_complete = right_record.state == NodeState::Complete;
                    right_complete
                        .cmp(&left_complete)
                        .then_with(|| size_for(right_record, size_mode).cmp(&size_for(left_record, size_mode)))
                        .then_with(|| tree.name(*left).unwrap_or_default().cmp(tree.name(*right).unwrap_or_default()))
                }
            }
        });
    }

    fn sort_visible_if_needed(&mut self) -> bool {
        if self.range_mode {
            self.message = Some("Finish range marking before changing row order.".to_owned());
            return true;
        }
        if self.sort_mode == SortMode::Size {
            let previous = self.cursor;
            self.sort_listing();
            self.rebuild_visible(true);
            self.cursor = self.valid_cursor(previous).unwrap_or_else(|| self.first_cursor());
            self.refresh_cursor_index();
        }
        true
    }

    fn set_sort(&mut self, sort: SortMode, phase: Phase) -> bool {
        if self.range_mode {
            self.message = Some("Finish range marking before changing row order.".to_owned());
            return true;
        }
        self.sort_mode = sort;
        if phase == Phase::Ready {
            let cursor = self.cursor;
            self.sort_listing();
            self.rebuild_visible(true);
            self.cursor = self.valid_cursor(cursor).unwrap_or_else(|| self.first_cursor());
            self.refresh_cursor_index();
        } else {
            self.message = Some("The chosen sort order will apply after scanning.".to_owned());
        }
        true
    }

    fn filter_matches(&self, id: NodeId) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let name = self.tree.name(id).unwrap_or_default();
        name.windows(self.filter.len())
            .any(|candidate| candidate.eq_ignore_ascii_case(self.filter.as_bytes()))
    }

    fn valid_cursor(&self, cursor: Cursor) -> Option<Cursor> {
        match cursor {
            Cursor::Parent if self.has_parent_row() => Some(Cursor::Parent),
            Cursor::Entry(id) if self.visible.contains(&id) => Some(Cursor::Entry(id)),
            _ => None,
        }
    }

    fn first_cursor(&self) -> Cursor {
        if let Some(first) = self.visible.first() {
            Cursor::Entry(*first)
        } else if self.has_parent_row() {
            Cursor::Parent
        } else {
            Cursor::None
        }
    }

    fn has_parent_row(&self) -> bool {
        self.current_directory != self.tree.root()
    }

    fn cursor_index(&self) -> Option<usize> {
        self.cursor_index
    }

    fn refresh_cursor_index(&mut self) {
        let offset = usize::from(self.has_parent_row());
        self.cursor_index = match self.cursor {
            Cursor::Parent if self.has_parent_row() => Some(0),
            Cursor::Entry(id) => self.visible.iter().position(|candidate| *candidate == id).map(|index| index + offset),
            Cursor::None => None,
            _ => None,
        };
    }

    fn move_cursor(&mut self, amount: isize) -> bool {
        let count = self.visible.len() + usize::from(self.has_parent_row());
        if count == 0 {
            return false;
        }
        let current = self.cursor_index().unwrap_or(0) as isize;
        let next = current.saturating_add(amount).clamp(0, count.saturating_sub(1) as isize) as usize;
        let changed = self.set_cursor_index(next);
        if self.range_mode {
            self.extend_range();
        }
        changed
    }

    fn move_to_edge(&mut self, end: bool) -> bool {
        let count = self.visible.len() + usize::from(self.has_parent_row());
        if count == 0 {
            return false;
        }
        let changed = self.set_cursor_index(if end { count - 1 } else { 0 });
        if self.range_mode {
            self.extend_range();
        }
        changed
    }

    fn set_cursor_index(&mut self, index: usize) -> bool {
        let offset = usize::from(self.has_parent_row());
        let next = if self.has_parent_row() && index == 0 {
            Cursor::Parent
        } else {
            self.visible
                .get(index.saturating_sub(offset))
                .copied()
                .map(Cursor::Entry)
                .unwrap_or(Cursor::None)
        };
        let changed = !same_cursor(self.cursor, next);
        self.cursor = next;
        self.cursor_index = Some(index);
        if changed && self.scanning {
            self.cursor_moved_during_scan = true;
        }
        changed
    }

    fn toggle_mark(&mut self) -> bool {
        let Cursor::Entry(id) = self.cursor else {
            return false;
        };
        let changed = if self.marks.remove(&id) {
            true
        } else if self.tree.record(id).is_some_and(|record| record.state != NodeState::Tombstone) {
            self.marks.insert(id)
        } else {
            false
        };
        self.move_cursor(1);
        changed
    }

    fn toggle_range(&mut self) -> bool {
        if self.range_mode {
            self.range_mode = false;
            self.range_anchor = None;
            self.message = None;
            return true;
        }
        let Cursor::Entry(id) = self.cursor else {
            return false;
        };
        self.range_anchor = Some(id);
        self.range_mode = true;
        self.marks.insert(id);
        true
    }

    fn extend_range(&mut self) {
        let (Some(anchor), Cursor::Entry(target)) = (self.range_anchor, self.cursor) else {
            return;
        };
        let Some(start) = self.visible.iter().position(|id| *id == anchor) else {
            return;
        };
        let Some(end) = self.visible.iter().position(|id| *id == target) else {
            return;
        };
        for id in &self.visible[start.min(end)..=start.max(end)] {
            self.marks.insert(*id);
        }
    }

    fn mark_all_visible(&mut self) -> bool {
        let before = self.marks.len();
        self.marks.extend(self.visible.iter().copied());
        self.marks.len() != before
    }

    fn enter_cursor(&mut self) -> bool {
        match self.cursor {
            Cursor::Parent => self.go_parent(),
            Cursor::Entry(id) => {
                let Some(record) = self.tree.record(id) else {
                    return false;
                };
                if record.entry_type != EntryType::Directory {
                    self.message = Some("This entry is not a directory.".to_owned());
                    return true;
                }
                match record.state {
                    NodeState::Excluded(ExclusionReason::MountBoundary) => {
                        self.message = Some("This directory is an excluded mount boundary.".to_owned());
                        return true;
                    }
                    NodeState::Excluded(ExclusionReason::UnsupportedAlias) => {
                        self.message = Some("This directory is an unsupported filesystem alias.".to_owned());
                        return true;
                    }
                    _ => {}
                }
                self.current_directory = id;
                self.cursor_moved_during_scan = false;
                self.marks.clear();
                self.range_mode = false;
                self.range_anchor = None;
                self.message = None;
                self.rebuild_listing(false);
                true
            }
            Cursor::None => false,
        }
    }

    fn go_parent(&mut self) -> bool {
        let Some(parent) = self.tree.record(self.current_directory).and_then(|record| record.parent()) else {
            return false;
        };
        self.current_directory = parent;
        self.cursor_moved_during_scan = false;
        self.marks.clear();
        self.range_mode = false;
        self.range_anchor = None;
        self.message = None;
        self.rebuild_listing(false);
        true
    }

    fn is_scanning(&self) -> bool {
        self.scanning
    }
}

fn same_cursor(left: Cursor, right: Cursor) -> bool {
    match (left, right) {
        (Cursor::Parent, Cursor::Parent) | (Cursor::None, Cursor::None) => true,
        (Cursor::Entry(left), Cursor::Entry(right)) => left == right,
        _ => false,
    }
}

fn size_for(record: &fdu_core::NodeRecord, mode: SizeMode) -> u64 {
    match mode {
        SizeMode::Allocated => record.allocated_bytes,
        SizeMode::Apparent => record.apparent_bytes,
    }
}

pub fn run(path: std::path::PathBuf, read_only: bool, apparent: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = TerminalSession::enter()?;
    let root = open_root(&path)?;
    let mut model = BrowserModel::new(&root, read_only, apparent)?;
    let profiling_enabled = std::env::var_os("FDU_PROFILE").is_some();
    let scan_metrics = ScanQueueMetrics::new(SCAN_EVENT_CHANNEL_CAPACITY, profiling_enabled);
    let mut profile = BrowserProfile::new(profiling_enabled, scan_metrics.clone());
    let mut phase = AppPhase::Scanning(ScanRun::start(root.clone(), scan_metrics));
    let mut last_draw = Instant::now() - SCAN_REDRAW_INTERVAL;
    let mut draw_pending = true;
    let mut first_live_listing_directory = None;
    loop {
        let text_input = matches!(&model.dialog, DialogState::Filter(_));
        let input_poll_interval = if matches!(&phase, AppPhase::Scanning(_)) {
            SCAN_INPUT_POLL_INTERVAL
        } else {
            INPUT_POLL_INTERVAL
        };
        let intent = fdu_tui::poll_intent(input_poll_interval, text_input)?;
        let mut input_changed = false;
        let mut action = Action::None;
        if let Some(intent) = intent {
            profile.note_input();
            let (next_action, changed) = model.handle_intent(intent, phase.view_phase());
            action = next_action;
            input_changed = changed;
        }
        match action {
            Action::None => {}
            Action::Quit => break,
            Action::Refresh => {
                model.reset_for_rescan(&root)?;
                first_live_listing_directory = None;
                let metrics = ScanQueueMetrics::new(SCAN_EVENT_CHANNEL_CAPACITY, profiling_enabled);
                profile.reset_scan_queue(metrics.clone());
                phase = AppPhase::Scanning(ScanRun::start(root.clone(), metrics));
                draw_pending = true;
            }
            Action::Delete(plan) => match root.try_clone_fd() {
                Ok(root_fd) => {
                    phase = AppPhase::Deleting(DeleteRun::start(root_fd, plan));
                    draw_pending = true;
                }
                Err(error) => {
                    model.message = Some(format!("Could not anchor deletion to the scan root: {error}"));
                    draw_pending = true;
                }
            },
            Action::CancelDeletion => {
                if let AppPhase::Deleting(worker) = &phase {
                    worker.cancel();
                    model.message = Some("Stopping after the current deletion operation.".to_owned());
                    draw_pending = true;
                }
            }
        }

        let mut model_changed = false;
        let mut scan_finished = false;
        if let AppPhase::Scanning(worker) = &mut phase {
            if let Some(receiver) = worker.receiver.as_ref() {
                for _ in 0..MAX_SCAN_EVENTS_PER_TICK {
                    match receiver.try_recv() {
                        Ok(event) => {
                            worker.queue_metrics.event_received();
                            if matches!(&event, ScanEvent::Finished) {
                                scan_finished = true;
                                break;
                            }
                            match event {
                                ScanEvent::Entries(batch) => {
                                    let batch = model.apply_entry_batch(batch);
                                    worker.return_batch(batch);
                                    model_changed = true;
                                }
                                other => model_changed |= model.apply_scan_event(other),
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            scan_finished = true;
                            break;
                        }
                    }
                }
            }
        }
        if scan_finished {
            let scan_error = match &mut phase {
                AppPhase::Scanning(worker) => worker.finish().err(),
                _ => None,
            };
            model.finish_scan(scan_error);
            phase = AppPhase::Ready;
            profile.note_scan_settled();
            model_changed = true;
        }

        let mut delete_finished = false;
        if let AppPhase::Deleting(worker) = &mut phase {
            if let Some(receiver) = worker.receiver.as_ref() {
                for _ in 0..MAX_DELETE_EVENTS_PER_TICK {
                    match receiver.try_recv() {
                        Ok(DeleteEvent::Progress { completed, total, current }) => {
                            worker.completed = completed;
                            worker.total = total;
                            worker.current = model
                                .tree
                                .name(current)
                                .map(fdu_tui::escape_name)
                                .unwrap_or_else(|| "entry".to_owned());
                            model_changed = true;
                        }
                        Ok(DeleteEvent::Outcomes(outcomes)) => {
                            worker.outcomes.extend(outcomes);
                        }
                        Ok(DeleteEvent::Failed { message }) => {
                            worker.failure = Some(message.clone());
                            model.message = Some(format!("Deletion stopped: {message}"));
                            model_changed = true;
                        }
                        Ok(DeleteEvent::Finished) => {
                            delete_finished = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            delete_finished = true;
                            break;
                        }
                    }
                }
            }
        }
        if delete_finished {
            let (outcomes, delete_error) = match &mut phase {
                AppPhase::Deleting(worker) => match worker.finish() {
                    Ok((outcomes, failure)) => (outcomes, failure),
                    Err(error) => (Vec::new(), Some(error)),
                },
                _ => (Vec::new(), None),
            };
            model.apply_delete_outcomes(outcomes, delete_error);
            phase = AppPhase::Ready;
            model_changed = true;
        }

        if input_changed || model_changed {
            draw_pending = true;
        }
        let first_live_listing_pending = !model.visible.is_empty()
            && first_live_listing_directory != Some(model.current_directory);
        let draw_now = match &phase {
            AppPhase::Scanning(_) => {
                draw_pending && (last_draw.elapsed() >= SCAN_REDRAW_INTERVAL || first_live_listing_pending)
                    || input_changed
            }
            AppPhase::Deleting(_) => draw_pending && last_draw.elapsed() >= SCAN_REDRAW_INTERVAL || input_changed,
            AppPhase::Ready => draw_pending,
        };
        if draw_now {
            let view = model.view(&phase);
            terminal.draw(&view)?;
            profile.note_render(&model, phase.view_phase());
            if !model.visible.is_empty() {
                first_live_listing_directory = Some(model.current_directory);
            }
            draw_pending = false;
            last_draw = Instant::now();
        }
    }
    drop(phase);
    terminal.restore();
    profile.report(&model);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{create_plan, Action, BrowserModel};
    use fdu_core::{DirectoryToken, EntryType, FileIdentity, NodeId, NodeState, ScanEvent};
    use fdu_delete::{DeleteOutcome, OutcomeKind};
    use fdu_scan::{open_root, RootAnchor};
    use fdu_tui::{Cursor, Intent, Phase, SizeMode, SortMode};
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> io::Result<Self> {
            loop {
                let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("fdu-browser-{}-{id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
        }

        fn root(&self) -> io::Result<RootAnchor> {
            open_root(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn append(
        model: &mut BrowserModel,
        parent: NodeId,
        name: &[u8],
        entry_type: EntryType,
        bytes: u64,
        inode: u64,
    ) -> NodeId {
        let state = NodeState::Complete;
        let tree = Arc::get_mut(&mut model.tree).expect("test model has a single tree owner");
        let id = tree
            .append(
                parent,
                name,
                entry_type,
                FileIdentity { device: 1, inode },
                1,
                if entry_type == EntryType::Directory { 0 } else { bytes },
                if entry_type == EntryType::Directory { 0 } else { bytes },
                state,
            )
            .unwrap();
        if entry_type != EntryType::Directory {
            assert!(tree.add_to_ancestors(parent, bytes, bytes));
        }
        model.indexed_entries = model.tree.len().saturating_sub(1);
        if parent == model.current_directory {
            model.listing.push(id);
            if model.filter_matches(id) {
                model.visible.push(id);
            }
            if matches!(model.cursor, Cursor::None) {
                model.cursor = Cursor::Entry(id);
                model.refresh_cursor_index();
            }
        }
        id
    }

    fn ready(model: &mut BrowserModel) {
        model.finish_scan(None);
    }

    fn make_model(temp: &TempDir, read_only: bool) -> io::Result<(RootAnchor, BrowserModel)> {
        let root = temp.root()?;
        let model = BrowserModel::new(&root, read_only, false)?;
        Ok((root, model))
    }

    #[test]
    fn finish_scan_selects_sorted_top_entry_when_cursor_was_untouched() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root = model.tree.root();
        append(&mut model, root, b"first-discovered", EntryType::RegularFile, 1, 2);
        let largest = append(&mut model, root, b"largest", EntryType::RegularFile, 100, 3);

        ready(&mut model);

        assert!(matches!(model.cursor, Cursor::Entry(id) if id == largest));
        assert_eq!(model.cursor_index(), Some(0));
        Ok(())
    }

    #[test]
    fn finish_scan_preserves_deliberate_cursor_movement_by_node_id() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root = model.tree.root();
        append(&mut model, root, b"first-discovered", EntryType::RegularFile, 1, 2);
        let selected = append(&mut model, root, b"selected", EntryType::RegularFile, 10, 3);
        append(&mut model, root, b"largest", EntryType::RegularFile, 100, 4);

        let (_, changed) = model.handle_intent(Intent::MoveDown, Phase::Scanning);
        assert!(changed);
        ready(&mut model);

        assert!(matches!(model.cursor, Cursor::Entry(id) if id == selected));
        assert_eq!(model.cursor_index(), Some(1));
        Ok(())
    }

    #[test]
    fn directory_local_marks_survive_filter_and_sort_and_clear_on_navigation_or_rescan() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (root, mut model) = make_model(&temp, false)?;
        let root_node = model.tree.root();
        let alpha = append(&mut model, root_node, b"alpha", EntryType::Directory, 0, 2);
        let nested = append(&mut model, alpha, b"nested", EntryType::Directory, 0, 3);
        append(&mut model, nested, b"payload", EntryType::RegularFile, 50, 4);
        let bravo = append(&mut model, root_node, b"bravo", EntryType::RegularFile, 100, 5);
        let charlie = append(&mut model, root_node, b"charlie", EntryType::RegularFile, 25, 6);
        ready(&mut model);

        model.marks.insert(alpha);
        model.marks.insert(bravo);
        model.filter = "charlie".to_owned();
        model.rebuild_visible(true);
        assert_eq!(model.visible, vec![charlie]);
        assert_eq!(model.marks.len(), 2);
        assert!(model.set_sort(SortMode::Name, Phase::Ready));
        assert_eq!(model.marks.len(), 2);

        model.filter.clear();
        model.rebuild_visible(true);
        model.cursor = Cursor::Entry(alpha);
        model.refresh_cursor_index();
        assert!(model.enter_cursor());
        assert!(model.marks.is_empty());
        assert_eq!(model.current_directory, alpha);
        assert_eq!(model.visible, vec![nested]);

        model.marks.insert(nested);
        model.reset_for_rescan(&root)?;
        assert!(model.marks.is_empty());
        assert_eq!(model.current_directory, model.tree.root());
        assert!(model.is_scanning());
        assert!(matches!(model.size_mode, SizeMode::Allocated));
        Ok(())
    }

    #[test]
    fn mark_all_captures_only_known_matching_entries_during_streaming() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root = model.tree.root();
        let alpha = append(&mut model, root, b"alpha", EntryType::RegularFile, 1, 2);
        let beta = append(&mut model, root, b"beta", EntryType::RegularFile, 1, 3);
        model.filter = "alpha".to_owned();
        model.rebuild_visible(true);
        assert!(model.mark_all_visible());
        assert!(model.marks.contains(&alpha));
        assert!(!model.marks.contains(&beta));

        let gamma = append(&mut model, root, b"gamma", EntryType::RegularFile, 1, 4);
        assert!(!model.marks.contains(&gamma));
        model.marks.insert(beta);
        assert_eq!(model.marks.len(), 2);
        assert_eq!(model.visible, vec![alpha]);
        Ok(())
    }

    #[test]
    fn scan_phase_rejects_delete_and_refresh_until_ready() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root_node = model.tree.root();
        let entry = append(&mut model, root_node, b"file", EntryType::RegularFile, 8, 2);
        let (action, changed) = model.handle_intent(Intent::Delete, Phase::Scanning);
        assert!(matches!(action, Action::None));
        assert!(changed);
        assert!(model.message.as_deref().unwrap().contains("unavailable"));
        let (action, _) = model.handle_intent(Intent::Refresh, Phase::Scanning);
        assert!(matches!(action, Action::None));

        ready(&mut model);
        model.cursor = Cursor::Entry(entry);
        let (action, _) = model.handle_intent(Intent::Delete, Phase::Ready);
        assert!(matches!(action, Action::None));
        let (action, _) = model.handle_intent(Intent::Enter, Phase::Ready);
        match action {
            Action::Delete(plan) => assert_eq!(plan.operation_count(), 1),
            _ => panic!("confirmation should freeze the selected deletion plan"),
        }

        let (_, mut read_only) = make_model(&temp, true)?;
        let read_only_root = read_only.tree.root();
        append(&mut read_only, read_only_root, b"file", EntryType::RegularFile, 8, 9);
        ready(&mut read_only);
        let (action, _) = read_only.handle_intent(Intent::Delete, Phase::Ready);
        assert!(matches!(action, Action::None));
        assert!(read_only.message.as_deref().unwrap().contains("read only"));
        Ok(())
    }

    #[test]
    fn indexed_directory_errors_keep_the_subtree_incomplete_but_allow_complete_leaves() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root = model.tree.root();
        let directory = append(&mut model, root, b"directory", EntryType::Directory, 0, 2);
        let file = append(&mut model, directory, b"complete-file", EntryType::RegularFile, 8, 3);
        model.token_nodes.push(Some(directory));

        assert!(model.apply_scan_event(ScanEvent::DirectoryFailed {
            directory: DirectoryToken(1),
            message: "permission denied".to_owned(),
        }));
        model.finish_scan(None);

        assert_eq!(model.scan_errors, 1);
        assert_eq!(model.incomplete_entries, 2);
        assert_eq!(model.tree.record(root).unwrap().state, NodeState::Incomplete);
        assert_eq!(model.tree.record(directory).unwrap().state, NodeState::Incomplete);
        assert_eq!(model.tree.record(file).unwrap().state, NodeState::Complete);
        assert!(create_plan(Arc::clone(&model.tree), &[file]).is_ok());
        let rejection = match create_plan(Arc::clone(&model.tree), &[directory]) {
            Ok(_) => panic!("incomplete directories cannot be planned for deletion"),
            Err(rejection) => rejection,
        };
        assert!(rejection.rejected[0].contains("scan is incomplete"));
        Ok(())
    }

    #[test]
    fn delete_outcomes_tombstone_postorder_once_and_adjust_entry_count() -> io::Result<()> {
        let temp = TempDir::new()?;
        let (_, mut model) = make_model(&temp, false)?;
        let root = model.tree.root();
        let directory = append(&mut model, root, b"dir", EntryType::Directory, 0, 2);
        let file = append(&mut model, directory, b"file", EntryType::RegularFile, 12, 3);
        ready(&mut model);
        assert_eq!(model.indexed_entries, 2);

        model.apply_delete_outcomes(
            vec![
                DeleteOutcome { node: file, kind: OutcomeKind::Deleted, message: None },
                DeleteOutcome { node: directory, kind: OutcomeKind::Deleted, message: None },
            ],
            None,
        );
        assert_eq!(model.indexed_entries, 0);
        assert_eq!(model.actual_deletions, 2);
        assert_eq!(model.tree.record(root).unwrap().apparent_bytes, 0);
        assert_eq!(model.tree.record(directory).unwrap().state, NodeState::Tombstone);
        assert!(model.visible.is_empty());
        Ok(())
    }
}

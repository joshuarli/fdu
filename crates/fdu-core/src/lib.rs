use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NodeId(u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn from_index(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryToken(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryType {
    Root,
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusionReason {
    MountBoundary,
    UnsupportedAlias,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeState {
    Scanning,
    Complete,
    Incomplete,
    Excluded(ExclusionReason),
    Stale,
    Tombstone,
}

#[derive(Clone, Debug)]
pub struct ScanEntry {
    pub name: Range<u32>,
    pub directory_token: Option<DirectoryToken>,
    pub entry_type: EntryType,
    pub identity: FileIdentity,
    pub link_count: u64,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub state: NodeState,
}

#[derive(Debug)]
pub struct EntryBatch {
    pub directory: DirectoryToken,
    pub entries: Vec<ScanEntry>,
    pub names: Vec<u8>,
}

impl EntryBatch {
    pub fn with_capacity(directory: DirectoryToken, entry_capacity: usize, name_capacity: usize) -> Self {
        Self {
            directory,
            entries: Vec::with_capacity(entry_capacity),
            names: Vec::with_capacity(name_capacity),
        }
    }

    pub fn clear_for_directory(&mut self, directory: DirectoryToken) {
        self.directory = directory;
        self.entries.clear();
        self.names.clear();
    }
}

#[derive(Debug)]
pub enum ScanEvent {
    Started {
        root_name: Vec<u8>,
        identity: FileIdentity,
        link_count: u64,
    },
    Entries(EntryBatch),
    DirectoryFinished {
        directory: DirectoryToken,
        complete: bool,
    },
    /// Completion updates grouped to reduce event traffic on scans with many directories.
    DirectoriesFinished(Vec<(DirectoryToken, bool)>),
    DirectoryExcluded {
        directory: DirectoryToken,
        reason: ExclusionReason,
    },
    DirectoryFailed {
        directory: DirectoryToken,
        message: String,
    },
    Failed {
        message: String,
    },
    Cancelled,
    Finished,
}

#[derive(Clone, Debug)]
pub struct NodeRecord {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    name: Range<u32>,
    pub entry_type: EntryType,
    pub identity: FileIdentity,
    pub link_count: u64,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub state: NodeState,
}

impl NodeRecord {
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }

    pub fn name_range(&self) -> Range<u32> {
        self.name.clone()
    }
}

pub struct Tree {
    nodes: Vec<NodeRecord>,
    names: Vec<u8>,
    root: NodeId,
}

impl Tree {
    pub fn new(
        root_name: &[u8],
        identity: FileIdentity,
        link_count: u64,
    ) -> Option<Self> {
        let root = NodeId(0);
        let name_end = u32::try_from(root_name.len()).ok()?;
        let mut names = Vec::with_capacity(root_name.len());
        names.extend_from_slice(root_name);
        Some(Self {
            nodes: vec![NodeRecord {
                parent: None,
                first_child: None,
                last_child: None,
                next_sibling: None,
                name: 0..name_end,
                entry_type: EntryType::Root,
                identity,
                link_count,
                apparent_bytes: 0,
                allocated_bytes: 0,
                state: NodeState::Scanning,
            }],
            names,
            root,
        })
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn retained_arena_bytes(&self) -> usize {
        self.nodes
            .capacity()
            .saturating_mul(std::mem::size_of::<NodeRecord>())
            .saturating_add(self.names.capacity())
    }

    pub fn record(&self, id: NodeId) -> Option<&NodeRecord> {
        self.nodes.get(id.index())
    }

    pub fn name(&self, id: NodeId) -> Option<&[u8]> {
        let range = self.record(id)?.name.clone();
        self.names.get(range.start as usize..range.end as usize)
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.record(id).and_then(NodeRecord::first_child),
        }
    }

    pub fn append(
        &mut self,
        parent: NodeId,
        name: &[u8],
        entry_type: EntryType,
        identity: FileIdentity,
        link_count: u64,
        apparent_bytes: u64,
        allocated_bytes: u64,
        state: NodeState,
    ) -> Option<NodeId> {
        let index = self.nodes.len();
        let id = NodeId::from_index(index)?;
        let name_start = u32::try_from(self.names.len()).ok()?;
        let name_end = name_start.checked_add(u32::try_from(name.len()).ok()?)?;
        if self.record(parent).is_none() {
            return None;
        }
        self.names.extend_from_slice(name);
        self.nodes.push(NodeRecord {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            next_sibling: None,
            name: name_start..name_end,
            entry_type,
            identity,
            link_count,
            apparent_bytes,
            allocated_bytes,
            state,
        });
        if let Some(last) = self.nodes[parent.index()].last_child {
            self.nodes[last.index()].next_sibling = Some(id);
        } else {
            self.nodes[parent.index()].first_child = Some(id);
        }
        self.nodes[parent.index()].last_child = Some(id);
        Some(id)
    }

    pub fn set_state(&mut self, id: NodeId, state: NodeState) -> bool {
        let Some(record) = self.nodes.get_mut(id.index()) else {
            return false;
        };
        record.state = state;
        true
    }

    pub fn add_to_ancestors(&mut self, id: NodeId, apparent: u64, allocated: u64) -> bool {
        let mut current = Some(id);
        let mut lineage = Vec::new();
        let mut fits = true;
        while let Some(node) = current {
            let Some(record) = self.nodes.get(node.index()) else {
                return false;
            };
            if record.apparent_bytes.checked_add(apparent).is_none()
                || record.allocated_bytes.checked_add(allocated).is_none()
            {
                fits = false;
            }
            lineage.push(node);
            current = record.parent;
        }
        if !fits {
            for ancestor in lineage {
                if let Some(record) = self.nodes.get_mut(ancestor.index()) {
                    record.state = NodeState::Incomplete;
                }
            }
            return false;
        }
        for node in lineage {
            let record = &mut self.nodes[node.index()];
            record.apparent_bytes += apparent;
            record.allocated_bytes += allocated;
        }
        true
    }

    pub fn mark_incomplete_to_root(&mut self, id: NodeId) {
        let mut current = Some(id);
        while let Some(node) = current {
            let Some(record) = self.nodes.get_mut(node.index()) else {
                break;
            };
            if record.state == NodeState::Scanning || record.state == NodeState::Complete {
                record.state = NodeState::Incomplete;
            }
            current = record.parent;
        }
    }

    pub fn materialize_path(&self, id: NodeId) -> Option<Vec<&[u8]>> {
        let mut names = Vec::new();
        let mut current = Some(id);
        while let Some(node) = current {
            let record = self.record(node)?;
            if node != self.root {
                names.push(self.name(node)?);
            }
            current = record.parent;
        }
        names.reverse();
        Some(names)
    }

    pub fn tombstone_subtree(&mut self, id: NodeId) -> Option<(u64, u64, usize)> {
        if id == self.root {
            return None;
        }
        let (apparent, allocated, parent) = {
            let record = self.record(id)?;
            if record.state == NodeState::Tombstone {
                return Some((0, 0, 0));
            }
            (record.apparent_bytes, record.allocated_bytes, record.parent?)
        };
        let mut current = Some(parent);
        let mut lineage = Vec::new();
        while let Some(node) = current {
            let record = self.nodes.get(node.index())?;
            if record.apparent_bytes < apparent || record.allocated_bytes < allocated {
                return None;
            }
            lineage.push(node);
            current = record.parent;
        }
        let mut child = self.record(id)?.first_child;
        let mut all_children_tombstoned = true;
        while let Some(child_id) = child {
            let child_record = self.record(child_id)?;
            if child_record.state != NodeState::Tombstone {
                all_children_tombstoned = false;
                break;
            }
            child = child_record.next_sibling;
        }

        let mut tombstoned = 0usize;
        let mut current = Some(id);
        while let Some(node) = current {
            let record = self.record(node)?;
            if record.state == NodeState::Tombstone {
                current = next_sibling_after_subtree(self, node, id);
                continue;
            }
            let next = if node == id && all_children_tombstoned {
                None
            } else if let Some(child) = record.first_child {
                Some(child)
            } else {
                next_sibling_after_subtree(self, node, id)
            };
            self.nodes.get_mut(node.index())?.state = NodeState::Tombstone;
            tombstoned = tombstoned.saturating_add(1);
            current = next;
        }
        for node in lineage {
            let record = &mut self.nodes[node.index()];
            record.apparent_bytes -= apparent;
            record.allocated_bytes -= allocated;
        }
        Some((apparent, allocated, tombstoned))
    }

    pub fn mark_stale_to_root(&mut self, id: NodeId) {
        let mut current = Some(id);
        while let Some(node) = current {
            let Some(record) = self.nodes.get_mut(node.index()) else {
                break;
            };
            if record.state != NodeState::Tombstone {
                record.state = NodeState::Stale;
            }
            current = record.parent;
        }
    }

    pub fn mark_scanning_incomplete(&mut self) -> usize {
        let mut changed = 0;
        for record in &mut self.nodes {
            if record.state == NodeState::Scanning {
                record.state = NodeState::Incomplete;
                changed += 1;
            }
        }
        changed
    }
}

fn next_sibling_after_subtree(tree: &Tree, node: NodeId, root: NodeId) -> Option<NodeId> {
    let mut current = node;
    loop {
        if current == root {
            return None;
        }
        let record = tree.record(current)?;
        if let Some(sibling) = record.next_sibling {
            return Some(sibling);
        }
        current = record.parent?;
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: Option<NodeId>,
}

impl<'a> Iterator for Children<'a> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(id) = self.next {
            let record = self.tree.record(id)?;
            self.next = record.next_sibling;
            if record.state != NodeState::Tombstone {
                return Some(id);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryType, FileIdentity, NodeState, Tree};

    #[test]
    fn names_and_sibling_links_share_the_arena_and_keep_stable_ids() {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 2).unwrap();
        let first = tree
            .append(
                tree.root(),
                b"first-\xff",
                EntryType::RegularFile,
                FileIdentity { device: 1, inode: 2 },
                1,
                7,
                512,
                NodeState::Complete,
            )
            .unwrap();
        let second = tree
            .append(
                tree.root(),
                b"second",
                EntryType::RegularFile,
                FileIdentity { device: 1, inode: 2 },
                2,
                7,
                512,
                NodeState::Complete,
            )
            .unwrap();
        assert_eq!(tree.name(first), Some(b"first-\xff".as_slice()));
        assert_eq!(tree.children(tree.root()).collect::<Vec<_>>(), vec![first, second]);
        assert_eq!(tree.record(first).unwrap().next_sibling(), Some(second));
    }

    #[test]
    fn checked_batch_totals_propagate_atomically_and_tombstones_subtract_once() {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let directory = tree
            .append(
                tree.root(),
                b"dir",
                EntryType::Directory,
                FileIdentity { device: 1, inode: 2 },
                2,
                0,
                0,
                NodeState::Complete,
            )
            .unwrap();
        let file = tree
            .append(
                directory,
                b"file",
                EntryType::RegularFile,
                FileIdentity { device: 1, inode: 3 },
                1,
                7,
                512,
                NodeState::Complete,
            )
            .unwrap();
        assert!(tree.add_to_ancestors(directory, 7, 512));
        assert_eq!(tree.record(tree.root()).unwrap().apparent_bytes, 7);
        assert_eq!(tree.tombstone_subtree(file), Some((7, 512, 1)));
        assert_eq!(tree.record(directory).unwrap().apparent_bytes, 0);
        assert_eq!(tree.tombstone_subtree(directory), Some((0, 0, 1)));
        assert_eq!(tree.tombstone_subtree(directory), Some((0, 0, 0)));
        assert_eq!(tree.record(tree.root()).unwrap().allocated_bytes, 0);
        assert!(tree.children(directory).next().is_none());
    }

    #[test]
    fn overflow_marks_every_ancestor_without_partially_adding_the_batch() {
        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 1 }, 1).unwrap();
        let directory = tree
            .append(
                tree.root(),
                b"dir",
                EntryType::Directory,
                FileIdentity { device: 1, inode: 2 },
                2,
                0,
                0,
                NodeState::Complete,
            )
            .unwrap();
        assert!(tree.add_to_ancestors(tree.root(), u64::MAX, u64::MAX));
        assert!(!tree.add_to_ancestors(directory, 1, 1));
        assert_eq!(tree.record(directory).unwrap().apparent_bytes, 0);
        assert_eq!(tree.record(tree.root()).unwrap().apparent_bytes, u64::MAX);
        assert_eq!(tree.record(directory).unwrap().state, NodeState::Incomplete);
        assert_eq!(tree.record(tree.root()).unwrap().state, NodeState::Incomplete);
    }
}

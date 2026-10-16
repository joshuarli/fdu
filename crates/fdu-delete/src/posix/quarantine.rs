use super::*;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;

#[cfg(test)]
static PROCESS_INHERITANCE_BARRIER: std::sync::RwLock<()> = std::sync::RwLock::new(());

const PREFIX: &[u8] = b".fdu-quarantine-v1-";
const MAGIC: &[u8] = b"fdu quarantine\0\x01";
const INTENT_MAGIC: &[u8] = b"fdu capture\0\x01";

pub(super) struct Session {
    root: OwnedFd,
    directory: OwnedFd,
    name: Vec<u8>,
    recovery_required: AtomicBool,
}

struct Intent {
    root: FileIdentity,
    parents: Vec<(Vec<u8>, FileIdentity)>,
    name: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn sync(fd: BorrowedFd<'_>) -> io::Result<()> {
    retry(|| fs::fsync(fd))
}

fn names(fd: BorrowedFd<'_>) -> io::Result<Vec<Vec<u8>>> {
    fs::Dir::read_from(fd)?.map(|entry| {
        let entry = entry?;
        Ok(entry.file_name().to_bytes().to_vec())
    }).filter(|entry| !matches!(entry, Ok(name) if name == b"." || name == b"..")).collect()
}

fn read(fd: BorrowedFd<'_>, name: &[u8]) -> io::Result<Vec<u8>> {
    let fd = retry(|| fs::openat(fd, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty()))?;
    let metadata = retry(|| fs::fstat(&fd))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile || metadata.st_size > 64 * 1024 * 1024 {
        return Err(invalid("recovery record is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd).take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn publish(fd: BorrowedFd<'_>, temporary: &[u8], name: &[u8], bytes: &[u8]) -> io::Result<()> {
    let record = retry(|| fs::openat(fd, temporary, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR))?;
    let mut file = std::fs::File::from(record);
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::renameat_with(fd, temporary, fd, name, fs::RenameFlags::NOREPLACE)?;
    sync(fd)
}

fn put_identity(bytes: &mut Vec<u8>, identity: FileIdentity) {
    bytes.extend_from_slice(&identity.device.to_le_bytes());
    bytes.extend_from_slice(&identity.inode.to_le_bytes());
}

impl Intent {
    fn for_node(tree: &Tree, node: NodeId) -> Self {
        let mut parents = Vec::new();
        let mut current = tree.record(node).and_then(NodeRecord::parent);
        while let Some(parent) = current {
            if parent == tree.root() { break; }
            let record = tree.record(parent).expect("indexed ancestor");
            parents.push((tree.name(parent).expect("indexed ancestor name").to_vec(), record.identity));
            current = record.parent();
        }
        parents.reverse();
        Self {
            root: tree.record(tree.root()).expect("indexed root").identity,
            parents,
            name: tree.name(node).expect("indexed target name").to_vec(),
        }
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = INTENT_MAGIC.to_vec();
        put_identity(&mut bytes, self.root);
        bytes.extend_from_slice(&u32::try_from(self.parents.len()).map_err(|_| invalid("recovery path is too deep"))?.to_le_bytes());
        for (name, identity) in &self.parents {
            put_name(&mut bytes, name)?;
            put_identity(&mut bytes, *identity);
        }
        put_name(&mut bytes, &self.name)?;
        Ok(bytes)
    }

    fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        if take(&mut bytes, INTENT_MAGIC.len())? != INTENT_MAGIC { return Err(invalid("unknown recovery intent version")); }
        let root = take_identity(&mut bytes)?;
        let count = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().expect("four bytes")) as usize;
        if count > bytes.len() / 21 { return Err(invalid("invalid recovery ancestor count")); }
        let mut parents = Vec::with_capacity(count);
        for _ in 0..count {
            parents.push((take_name(&mut bytes)?, take_identity(&mut bytes)?));
        }
        let name = take_name(&mut bytes)?;
        if !bytes.is_empty() { return Err(invalid("trailing recovery record data")); }
        Ok(Self { root, parents, name })
    }

    fn parent(&self, root: BorrowedFd<'_>, mount: &MountIdentity) -> io::Result<OwnedFd> {
        let (found, found_mount) = directory_state(root)?;
        if found != self.root || found_mount != *mount { return Err(invalid("recovery root identity changed")); }
        let mut fd = retry(|| fs::openat(root, c".", DIRECTORY_FLAGS, Mode::empty()))?;
        for (name, expected) in &self.parents {
            let next = retry(|| fs::openat(&fd, name.as_slice(), DIRECTORY_FLAGS, Mode::empty()))?;
            let (found, found_mount) = directory_state(next.as_fd())?;
            if found != *expected || found_mount != *mount { return Err(invalid("recovery parent identity or mount changed")); }
            fd = next;
        }
        Ok(fd)
    }
}

fn put_name(bytes: &mut Vec<u8>, name: &[u8]) -> io::Result<()> {
    bytes.extend_from_slice(&u32::try_from(name.len()).map_err(|_| invalid("recovery name is too long"))?.to_le_bytes());
    bytes.extend_from_slice(name);
    Ok(())
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
    if bytes.len() < len { return Err(invalid("truncated recovery record")); }
    let (value, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(value)
}

fn take_identity(bytes: &mut &[u8]) -> io::Result<FileIdentity> {
    let device = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    let inode = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    Ok(FileIdentity { device, inode })
}

fn take_name(bytes: &mut &[u8]) -> io::Result<Vec<u8>> {
    let len = u32::from_le_bytes(take(bytes, 4)?.try_into().expect("four bytes")) as usize;
    let name = take(bytes, len)?;
    if name.is_empty() || name == b"." || name == b".." || name.contains(&0) || name.contains(&b'/') {
        return Err(invalid("invalid recovery path component"));
    }
    Ok(name.to_vec())
}

fn lock(root: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let fd = retry(|| fs::openat(root, c".", DIRECTORY_FLAGS, Mode::empty()))?;
    fs::flock(&fd, fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|error| io::Error::new(io::Error::from(error).kind(), "another fdu deletion or recovery owns this root"))?;
    Ok(fd)
}

#[cfg(test)]
fn recover_interrupted(root: &OwnedFd) -> io::Result<bool> {
    #[cfg(test)]
    let _process_inheritance = PROCESS_INHERITANCE_BARRIER.read().expect("process inheritance barrier");
    let root = lock(root.as_fd())?;
    recover(root.as_fd())
}

fn recover(root: BorrowedFd<'_>) -> io::Result<bool> {
    let (root_identity, mount) = directory_state(root)?;
    let mut recovered = false;
    for name in names(root)? {
        if !name.starts_with(PREFIX) { continue; }
        let directory = match fs::openat(root, name.as_slice(), DIRECTORY_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOTDIR | Errno::LOOP | Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        let header = match read(directory.as_fd(), b"header") {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(retained(&name, None, invalid("ambiguous quarantine directory without a header; move an unrelated lookalike out of this prefix")));
            },
            Err(error) => return Err(retained(&name, None, error)),
        };
        if !header.starts_with(MAGIC) { return Err(retained(&name, None, invalid("unknown or damaged quarantine header; move an unrelated lookalike out of this prefix"))); }
        let mut header_data = &header[MAGIC.len()..];
        let recorded_root = take_identity(&mut header_data).map_err(|error| retained(&name, None, error))?;
        if recorded_root != root_identity || !header_data.is_empty() || directory_state(directory.as_fd())?.1 != mount {
            return Err(retained(&name, None, invalid("quarantine root identity, format, or mount changed")));
        }
        recovered = true;
        recover_directory(root, directory.as_fd(), &name, &mount)?;
        fs::unlinkat(directory.as_fd(), b"header".as_slice(), AtFlags::empty())?;
        sync(directory.as_fd())?;
        fs::unlinkat(root, name.as_slice(), AtFlags::REMOVEDIR)?;
        sync(root)?;
    }
    Ok(recovered)
}

fn retained(workspace: &[u8], capture: Option<&[u8]>, error: io::Error) -> io::Error {
    let mut location = std::path::PathBuf::from(OsStr::from_bytes(workspace));
    if let Some(capture) = capture { location.push(OsStr::from_bytes(capture)); }
    io::Error::new(error.kind(), format!("{error}; retained at scan root/{}; restore it without overwriting the original name, then rescan", location.display()))
}

fn recover_directory(root: BorrowedFd<'_>, directory: BorrowedFd<'_>, workspace: &[u8], mount: &MountIdentity) -> io::Result<()> {
    let entries = names(directory)?;
    for name in &entries {
        if name == b"header" || name.starts_with(b"pending-") { continue; }
        let Some(id) = name.strip_suffix(b".intent") else {
            if let Some(id) = name.strip_suffix(b".entry") {
                let mut record = id.to_vec(); record.extend_from_slice(b".intent");
                if entries.contains(&record) { continue; }
            }
            return Err(retained(workspace, Some(name), invalid("unrecognized quarantine entry")));
        };
        if id.is_empty() || !id.iter().all(u8::is_ascii_digit) { return Err(retained(workspace, Some(name), invalid("invalid capture identifier"))); }
        let mut capture = id.to_vec(); capture.extend_from_slice(b".entry");
        let intent = Intent::decode(&read(directory, name).map_err(|error| retained(workspace, Some(&capture), error))?).map_err(|error| retained(workspace, Some(&capture), error))?;
        match stat_at(directory, &capture) {
            Ok(_) => {
                let parent = intent.parent(root, mount).map_err(|error| retained(workspace, Some(&capture), error))?;
                fs::renameat_with(directory, capture.as_slice(), &parent, intent.name.as_slice(), fs::RenameFlags::NOREPLACE)
                    .map_err(|error| retained(workspace, Some(&capture), error.into()))?;
                sync(parent.as_fd()).map_err(|error| retained(workspace, None, error))?;
                sync(directory).map_err(|error| retained(workspace, None, error))?;
            }
            Err(Errno::NOENT) => {}
            Err(error) => return Err(retained(workspace, Some(&capture), error.into())),
        }
        fs::unlinkat(directory, name.as_slice(), AtFlags::empty())?;
        sync(directory)?;
    }
    for name in entries.into_iter().filter(|name| name.starts_with(b"pending-")) {
        fs::unlinkat(directory, name.as_slice(), AtFlags::empty())?;
        sync(directory)?;
    }
    Ok(())
}

fn cleanup_initialization(root: BorrowedFd<'_>, name: &[u8]) -> io::Result<()> {
    let directory = retry(|| fs::openat(root, name, DIRECTORY_FLAGS, Mode::empty()))?;
    for artifact in [b"pending-header".as_slice(), b"header"] {
        match fs::unlinkat(&directory, artifact, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
    }
    fs::unlinkat(root, name, AtFlags::REMOVEDIR)?;
    sync(root)
}

impl Session {
    pub(super) fn start(root: BorrowedFd<'_>) -> io::Result<Self> {
        #[cfg(test)]
        let _process_inheritance = PROCESS_INHERITANCE_BARRIER.read().expect("process inheritance barrier");
        let root = lock(root)?;
        if recover(root.as_fd())? {
            return Err(io::Error::other("interrupted deletion recovered; rescan before deleting"));
        }
        let mut random = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let mut name = PREFIX.to_vec();
        for byte in random { name.extend_from_slice(format!("{byte:02x}").as_bytes()); }
        fs::mkdirat(&root, name.as_slice(), Mode::RWXU)?;
        let initialized = (|| {
            let directory = retry(|| fs::openat(&root, name.as_slice(), DIRECTORY_FLAGS, Mode::empty()))?;
            if directory_state(directory.as_fd())?.1 != directory_state(root.as_fd())?.1 { return Err(invalid("quarantine is on another mount")); }
            let mut header = MAGIC.to_vec();
            put_identity(&mut header, directory_state(root.as_fd())?.0);
            publish(directory.as_fd(), b"pending-header", b"header", &header)?;
            sync(root.as_fd())?;
            Ok(directory)
        })();
        let directory = match initialized {
            Ok(directory) => directory,
            Err(error) => {
                if let Err(cleanup_error) = cleanup_initialization(root.as_fd(), &name) {
                    return Err(retained(&name, None, io::Error::new(error.kind(), format!("quarantine initialization failed: {error}; cleanup failed: {cleanup_error}"))));
                }
                return Err(error);
            }
        };
        Ok(Self { root, directory, name, recovery_required: AtomicBool::new(false) })
    }

    pub(super) fn remove(&self, deletion: &Deletion<'_>, parent: BorrowedFd<'_>, name: &[u8], node: NodeId, flags: AtFlags) -> (OutcomeKind, Option<String>) {
        match self.capture_remove(deletion, parent, name, node, flags) {
            Ok(kind) => (kind, None),
            Err(error) => {
                let record = format!("{}.intent", node.index());
                let pending = format!("pending-{}", node.index());
                let unresolved = !matches!(stat_at(self.directory.as_fd(), record.as_bytes()), Err(Errno::NOENT))
                    || !matches!(stat_at(self.directory.as_fd(), pending.as_bytes()), Err(Errno::NOENT));
                if unresolved {
                    self.recovery_required.store(true, Ordering::Relaxed);
                    (OutcomeKind::Failed, Some(retained(&self.name, None, error).to_string()))
                } else {
                    (OutcomeKind::Failed, Some(error.to_string()))
                }
            }
        }
    }

    fn capture_remove(&self, deletion: &Deletion<'_>, parent: BorrowedFd<'_>, name: &[u8], node: NodeId, flags: AtFlags) -> io::Result<OutcomeKind> {
        let capture = format!("{}.entry", node.index()).into_bytes();
        let record = format!("{}.intent", node.index()).into_bytes();
        let pending = format!("pending-{}", node.index()).into_bytes();
        let intent = Intent::for_node(deletion.tree, node);
        let expected_parent = deletion.tree.record(node).and_then(NodeRecord::parent).and_then(|node| deletion.tree.record(node)).expect("indexed parent").identity;
        let (found, mount) = directory_state(parent)?;
        if found != expected_parent || mount != deletion.root_mount { return Err(invalid("source parent identity or mount changed")); }
        publish(self.directory.as_fd(), &pending, &record, &intent.encode()?)?;
        #[cfg(test)]
        (deletion.before_capture)(node);
        match fs::renameat_with(parent, name, &self.directory, capture.as_slice(), fs::RenameFlags::NOREPLACE) {
            Ok(()) => {}
            Err(Errno::NOENT) => { self.clear_record(&record)?; return Ok(OutcomeKind::AlreadyAbsent); }
            Err(error) => return Err(retained(&self.name, Some(&capture), error.into())),
        }
        sync(parent).map_err(|error| retained(&self.name, Some(&capture), error))?;
        sync(self.directory.as_fd()).map_err(|error| retained(&self.name, Some(&capture), error))?;
        let expected = deletion.tree.record(node).expect("indexed target");
        let captured = stat_at(self.directory.as_fd(), &capture).map_err(|error| retained(&self.name, Some(&capture), error.into()))?;
        if identity(&captured)? != expected.identity || entry_type(&captured) != expected.entry_type {
            self.restore(parent, name, &capture, &record)?;
            return Ok(OutcomeKind::Changed);
        }
        match fs::unlinkat(&self.directory, capture.as_slice(), flags) {
            Ok(()) => {
                sync(self.directory.as_fd()).map_err(|error| retained(&self.name, Some(&capture), error))?;
                self.clear_record(&record)?;
                Ok(OutcomeKind::Deleted)
            }
            Err(error) => {
                if let Err(restore_error) = self.restore(parent, name, &capture, &record) {
                    return Err(io::Error::new(restore_error.kind(), format!("capture removal failed: {}; restoration failed: {restore_error}", io::Error::from(error))));
                }
                if error == Errno::NOTEMPTY { return Err(io::Error::other("directory gained unindexed children; original directory restored")); }
                Err(error.into())
            }
        }
    }

    fn restore(&self, parent: BorrowedFd<'_>, name: &[u8], capture: &[u8], record: &[u8]) -> io::Result<()> {
        fs::renameat_with(&self.directory, capture, parent, name, fs::RenameFlags::NOREPLACE)
            .map_err(|error| retained(&self.name, Some(capture), error.into()))?;
        sync(parent).map_err(|error| retained(&self.name, None, error))?;
        sync(self.directory.as_fd()).map_err(|error| retained(&self.name, None, error))?;
        self.clear_record(record)
    }

    fn clear_record(&self, record: &[u8]) -> io::Result<()> {
        fs::unlinkat(&self.directory, record, AtFlags::empty())?;
        sync(self.directory.as_fd())
    }

    pub(super) fn recovery_required(&self) -> bool {
        self.recovery_required.load(Ordering::Relaxed)
    }

    pub(super) fn finish(&self) -> io::Result<()> {
        if names(self.directory.as_fd())?.iter().any(|name| name != b"header") { return Ok(()); }
        fs::unlinkat(&self.directory, b"header".as_slice(), AtFlags::empty())?;
        sync(self.directory.as_fd())?;
        fs::unlinkat(&self.root, self.name.as_slice(), AtFlags::REMOVEDIR)?;
        sync(self.root.as_fd())
    }
}

#[cfg(test)]
impl Drop for Session {
    fn drop(&mut self) {
        let _process_inheritance = PROCESS_INHERITANCE_BARRIER.read().expect("process inheritance barrier");
        fs::flock(&self.root, fs::FlockOperation::Unlock).expect("release test session lock before inherited descriptors close");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> io::Result<Self> {
            let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/tmp");
            std::fs::create_dir_all(&scratch)?;
            let path = scratch.join(format!("recovery-{}-{}", std::process::id(), NEXT_ROOT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn root(&self) -> io::Result<OwnedFd> {
            Ok(fs::open(&self.0, DIRECTORY_FLAGS, Mode::empty())?)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) { eprintln!("fixture cleanup failed: {error}"); }
        }
    }

    fn interrupt(path: &Path, stage: &str) -> io::Result<()> {
        let inheritance = PROCESS_INHERITANCE_BARRIER.write().expect("process inheritance barrier");
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", "posix::quarantine::tests::crash_child", "--ignored", "--nocapture"])
            .env("FDU_RECOVERY_TEST_ROOT", path)
            .env("FDU_RECOVERY_TEST_STAGE", stage)
            .spawn()?;
        drop(inheritance);
        let status = child.wait()?;
        assert_eq!(status.code(), Some(79));
        Ok(())
    }

    #[test]
    #[ignore]
    fn crash_child() -> io::Result<()> {
        let Some(path) = std::env::var_os("FDU_RECOVERY_TEST_ROOT") else { return Ok(()); };
        let stage = std::env::var("FDU_RECOVERY_TEST_STAGE").expect("test stage");
        let root = fs::open(Path::new(&path), DIRECTORY_FLAGS, Mode::empty())?;
        if stage == "initial-directory" {
            fs::mkdirat(&root, ".fdu-quarantine-v1-initial", Mode::RWXU)?;
            sync(root.as_fd())?;
            std::process::exit(79);
        }
        let session = Session::start(root.as_fd())?;
        if stage == "final-header" {
            fs::unlinkat(&session.directory, "header", AtFlags::empty())?;
            sync(session.directory.as_fd())?;
            std::process::exit(79);
        }
        let parent = fs::openat(&root, "parent", DIRECTORY_FLAGS, Mode::empty())?;
        let parent_identity = directory_state(parent.as_fd())?.0;
        let intent = Intent { root: directory_state(root.as_fd())?.0, parents: vec![(b"parent".to_vec(), parent_identity)], name: b"target".to_vec() };
        if stage == "partial" {
            let pending = fs::openat(&session.directory, "pending-1", OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY, Mode::RUSR | Mode::WUSR)?;
            let mut file = std::fs::File::from(pending);
            file.write_all(b"partial")?;
            file.sync_all()?;
            sync(session.directory.as_fd())?;
            std::process::exit(79);
        }
        publish(session.directory.as_fd(), b"pending-1", b"1.intent", &intent.encode()?)?;
        if stage == "intent" { std::process::exit(79); }
        if stage == "mismatch" {
            fs::renameat(&parent, "target", &parent, "indexed-original")?;
            std::fs::write(Path::new(&path).join("parent/target"), b"unindexed replacement")?;
        }
        fs::renameat_with(&parent, "target", &session.directory, "1.entry", fs::RenameFlags::NOREPLACE)?;
        if stage == "capture-unsynced" { std::process::exit(79); }
        sync(parent.as_fd())?;
        sync(session.directory.as_fd())?;
        if stage == "remove" {
            fs::unlinkat(&session.directory, "1.entry", AtFlags::empty())?;
            sync(session.directory.as_fd())?;
        }
        if stage == "restore" {
            fs::renameat_with(&session.directory, "1.entry", &parent, "target", fs::RenameFlags::NOREPLACE)?;
            sync(parent.as_fd())?;
            sync(session.directory.as_fd())?;
        }
        std::process::exit(79);
    }

    fn payload_fixture() -> io::Result<Fixture> {
        let fixture = Fixture::new()?;
        std::fs::create_dir(fixture.0.join("parent"))?;
        std::fs::write(fixture.0.join("parent/target"), b"indexed payload")?;
        Ok(fixture)
    }

    #[test]
    fn recovery_restores_captures_across_process_exit_windows() -> io::Result<()> {
        for stage in ["partial", "intent", "capture-unsynced", "capture", "restore", "remove", "mismatch"] {
            let fixture = payload_fixture()?;
            interrupt(&fixture.0, stage)?;
            let root = fixture.root()?;
            assert!(recover_interrupted(&root)?, "{stage}");
            if stage == "remove" {
                assert!(!fixture.0.join("parent/target").exists());
            } else if stage == "mismatch" {
                assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"unindexed replacement");
                assert_eq!(std::fs::read(fixture.0.join("parent/indexed-original"))?, b"indexed payload");
            } else {
                assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"indexed payload");
            }
            assert!(!recover_interrupted(&root)?);
            assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 1);
        }
        Ok(())
    }

    #[test]
    fn headerless_crash_remnants_refuse_deletion_without_losing_source_data() -> io::Result<()> {
        for stage in ["initial-directory", "final-header"] {
            let fixture = payload_fixture()?;
            interrupt(&fixture.0, stage)?;
            let error = recover_interrupted(&fixture.root()?).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("without a header"));
            assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"indexed payload");
            let workspace = std::fs::read_dir(&fixture.0)?.filter_map(Result::ok).find(|entry| entry.file_name().as_bytes().starts_with(PREFIX)).expect("workspace retained");
            assert_eq!(std::fs::read_dir(workspace.path())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn recovery_conflicts_retain_both_names_and_report_capture() -> io::Result<()> {
        let fixture = payload_fixture()?;
        interrupt(&fixture.0, "capture")?;
        std::fs::write(fixture.0.join("parent/target"), b"concurrent source")?;
        let error = recover_interrupted(&fixture.root()?).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("1.entry"));
        assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"concurrent source");
        let workspace = std::fs::read_dir(&fixture.0)?.filter_map(Result::ok).find(|entry| entry.file_name().as_bytes().starts_with(PREFIX)).expect("workspace retained");
        assert_eq!(std::fs::read(workspace.path().join("1.entry"))?, b"indexed payload");
        std::fs::remove_file(fixture.0.join("parent/target"))?;
        assert!(recover_interrupted(&fixture.root()?)?);
        assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"indexed payload");
        Ok(())
    }

    #[test]
    fn recovery_refuses_replaced_parent_and_never_follows_symlink() -> io::Result<()> {
        for symlink in [false, true] {
            let fixture = payload_fixture()?;
            interrupt(&fixture.0, "capture")?;
            std::fs::rename(fixture.0.join("parent"), fixture.0.join("moved"))?;
            if symlink {
                std::os::unix::fs::symlink("moved", fixture.0.join("parent"))?;
            } else {
                std::fs::create_dir(fixture.0.join("parent"))?;
            }
            assert!(recover_interrupted(&fixture.root()?).is_err());
            assert!(!fixture.0.join("parent/target").exists());
            assert!(!fixture.0.join("moved/target").exists());
        }
        Ok(())
    }

    #[test]
    fn active_session_allows_independent_crash_subprocess() -> io::Result<()> {
        let fixture = payload_fixture()?;
        let child_fixture = payload_fixture()?;
        let root = fixture.root()?;
        let session = Session::start(root.as_fd())?;
        std::thread::scope(|scope| {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let worker = scope.spawn(move || {
                let result = interrupt(&child_fixture.0, "intent");
                sender.send(result).expect("subprocess result receiver");
            });
            let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
            session.finish()?;
            drop(session);
            worker.join().expect("subprocess owner exits");
            result.map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
            Ok(())
        })
    }

    #[test]
    fn active_session_excludes_recovery_by_another_owner() -> io::Result<()> {
        let fixture = payload_fixture()?;
        let root = fixture.root()?;
        let session = Session::start(root.as_fd())?;
        assert_eq!(recover_interrupted(&root).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        session.finish()?;
        drop(session);
        assert!(!recover_interrupted(&root)?);
        Ok(())
    }

    #[test]
    fn recovery_retains_corrupt_records_and_unrelated_lookalikes() -> io::Result<()> {
        let fixture = payload_fixture()?;
        interrupt(&fixture.0, "capture")?;
        let workspace = std::fs::read_dir(&fixture.0)?.filter_map(Result::ok).find(|entry| entry.file_name().as_bytes().starts_with(PREFIX)).expect("workspace retained");
        std::fs::write(workspace.path().join("1.intent"), b"truncated")?;
        assert_eq!(recover_interrupted(&fixture.root()?).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(workspace.path().join("1.entry"))?, b"indexed payload");
        let lookalike = Fixture::new()?;
        let user_data = lookalike.0.join(".fdu-quarantine-v1-user");
        std::fs::create_dir(&user_data)?;
        std::fs::write(user_data.join("important"), b"user data")?;
        assert!(recover_interrupted(&lookalike.root()?).is_err());
        assert_eq!(std::fs::read(user_data.join("important"))?, b"user data");
        Ok(())
    }
    #[test]
    fn explicit_deletion_recovers_then_rejects_the_stale_plan() -> io::Result<()> {
        let fixture = payload_fixture()?;
        interrupt(&fixture.0, "capture")?;
        let root = fixture.root()?;
        let metadata = std::fs::metadata(fixture.0.join("parent"))?;
        use std::os::unix::fs::MetadataExt;
        let mut tree = Tree::new(fixture.0.as_os_str().as_bytes(), directory_state(root.as_fd())?.0, 1).expect("root index");
        let selected = tree.append(tree.root(), b"parent", EntryType::Directory, FileIdentity { device: metadata.dev(), inode: metadata.ino() }, metadata.nlink(), 0, 0, fdu_core::NodeStatus::Complete).expect("parent index");
        tree.set_state(tree.root(), fdu_core::NodeStatus::Complete);
        let plan = create_plan(Arc::new(tree), &[selected]).unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        run(root, plan, Arc::new(AtomicBool::new(false)), sender);
        match receiver.recv().expect("failure event") {
            DeleteEvent::Failed { message } => assert!(message.contains("rescan")),
            event => panic!("expected stale-plan rejection, got {event:?}"),
        }
        assert!(matches!(receiver.recv().expect("finish event"), DeleteEvent::Finished));
        assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"indexed payload");
        assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 1);
        Ok(())
    }

    #[test]
    fn mismatch_restoration_conflict_retains_both_entries() -> io::Result<()> {
        let fixture = payload_fixture()?;
        let root = fixture.root()?;
        let session = Session::start(root.as_fd())?;
        let parent = fs::openat(&root, "parent", DIRECTORY_FLAGS, Mode::empty())?;
        let intent = Intent { root: directory_state(root.as_fd())?.0, parents: vec![(b"parent".to_vec(), directory_state(parent.as_fd())?.0)], name: b"target".to_vec() };
        publish(session.directory.as_fd(), b"pending-1", b"1.intent", &intent.encode()?)?;
        fs::renameat_with(&parent, "target", &session.directory, "1.entry", fs::RenameFlags::NOREPLACE)?;
        std::fs::write(fixture.0.join("parent/target"), b"concurrent replacement")?;
        let error = session.restore(parent.as_fd(), b"target", b"1.entry", b"1.intent").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("1.entry"));
        assert_eq!(std::fs::read(fixture.0.join("parent/target"))?, b"concurrent replacement");
        assert_eq!(read(session.directory.as_fd(), b"1.entry")?, b"indexed payload");
        assert!(stat_at(session.directory.as_fd(), b"1.intent").is_ok());
        Ok(())
    }

    #[test]
    fn recovery_rejects_invalid_record_paths_and_unknown_versions() -> io::Result<()> {
        let intent = Intent { root: FileIdentity { device: 1, inode: 2 }, parents: Vec::new(), name: b"target".to_vec() };
        let valid = intent.encode()?;
        for len in 0..valid.len() { assert!(Intent::decode(&valid[..len]).is_err()); }
        let mut wrong_version = valid.clone();
        wrong_version[INTENT_MAGIC.len() - 1] = 2;
        assert!(Intent::decode(&wrong_version).is_err());
        let mut trailing = valid.clone(); trailing.push(0);
        assert!(Intent::decode(&trailing).is_err());
        for name in [b"..".as_slice(), b".", b"", b"a/b", b"a\0b"] {
            let invalid = Intent { root: intent.root, parents: Vec::new(), name: name.to_vec() }.encode()?;
            assert!(Intent::decode(&invalid).is_err());
        }
        Ok(())
    }

}

use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use zeppelin_embed::vfs::crash::{CrashVfs, MemoryVfs};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

use super::profiles::FaultProfile;
use super::program::{CrashBoundary, Op, Program};
use super::test_support;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultMode {
    Eio,
    Eacces,
    Enospc,
    BitFlip,
    TornWrite,
    Truncate,
    WrongObject,
    MisdirectedWrite,
    ZeroFill,
    Latency,
    SilentDrop,
    PostCommitError,
}

impl FaultMode {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Eio => "eio",
            Self::Eacces => "eacces",
            Self::Enospc => "enospc",
            Self::BitFlip => "bit_flip",
            Self::TornWrite => "torn_write",
            Self::Truncate => "truncate",
            Self::WrongObject => "wrong_object",
            Self::MisdirectedWrite => "misdirected_write",
            Self::ZeroFill => "zero_fill",
            Self::Latency => "latency",
            Self::SilentDrop => "silent_drop",
            Self::PostCommitError => "post_commit_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultSite {
    Open,
    Read,
    ReadRange,
    Write,
    Append,
    Sync,
    Rename,
    List,
    Delete,
    Clock,
}

impl FaultSite {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::ReadRange => "read_range",
            Self::Write => "write",
            Self::Append => "append",
            Self::Sync => "sync",
            Self::Rename => "rename",
            Self::List => "list",
            Self::Delete => "delete",
            Self::Clock => "clock",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultEvent {
    pub id: String,
    pub op_index: usize,
    pub site: FaultSite,
    pub mode: FaultMode,
    pub nth_match: usize,
    pub path_contains: Option<String>,
    pub fired: bool,
    pub path: Option<PathBuf>,
}

impl FaultEvent {
    #[must_use]
    pub fn json_line(&self) -> String {
        let path = self.path.as_ref().map_or_else(
            || "null".to_owned(),
            |path| format!("\"{}\"", json_escape(&path.display().to_string())),
        );
        format!(
            "{{\"id\":\"{}\",\"op\":{},\"site\":\"{}\",\"mode\":\"{}\",\"nth_match\":{},\"path_contains\":{},\"fired\":{},\"path\":{path}}}",
            self.id,
            self.op_index,
            self.site.key(),
            self.mode.key(),
            self.nth_match,
            self.path_contains.as_ref().map_or_else(
                || "null".to_owned(),
                |value| format!("\"{}\"", json_escape(value)),
            ),
            self.fired
        )
    }
}

#[derive(Clone, Debug, Default)]
struct Runtime {
    matches: usize,
    fired: bool,
    path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ScheduledVfs<V> {
    inner: V,
    event: Option<FaultEvent>,
    runtime: Arc<Mutex<Runtime>>,
    current_operation: Arc<AtomicUsize>,
}

/// A child-process-only VFS that terminates the process at one named product
/// durability boundary. The parent then reopens the same directory and checks
/// it against the logical model.
pub struct ProcessCrashVfs<V> {
    inner: V,
    boundary: CrashBoundary,
    armed: Arc<AtomicBool>,
}

impl<V> ProcessCrashVfs<V> {
    #[must_use]
    pub fn new(inner: V, boundary: CrashBoundary) -> Self {
        Self {
            inner,
            boundary,
            armed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn armed_for(&self, boundary: CrashBoundary) -> bool {
        self.boundary == boundary && self.armed.load(Ordering::SeqCst)
    }
}

struct ProcessCrashFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    boundary: CrashBoundary,
    armed: Arc<AtomicBool>,
}

impl ProcessCrashFile {
    fn should_crash_mid_wal(&self) -> bool {
        self.boundary == CrashBoundary::MidWalGroup
            && self.armed.load(Ordering::SeqCst)
            && file_name_is(&self.path, "wal.ze")
    }

    fn append_then_crash(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // Stop after the WAL group bytes reach the file but before its
        // durability sync/ack. Recovery must use the durable boundary and
        // therefore expose a clean pre-group prefix.
        self.inner.append(bytes)?;
        std::process::abort();
    }
}

impl VfsFile for ProcessCrashFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.should_crash_mid_wal() {
            return self.append_then_crash(bytes);
        }
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        if self.should_crash_mid_wal() {
            let bytes = buffers
                .iter()
                .flat_map(|buffer| buffer.as_ref().iter().copied())
                .collect::<Vec<_>>();
            return self.append_then_crash(&bytes);
        }
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

impl<V: Vfs> Vfs for ProcessCrashVfs<V> {
    fn segment_data_read_counter(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if self.armed_for(CrashBoundary::MidSeal) && is_segment_temp(path) {
            let persisted = bytes.len().div_ceil(2);
            self.inner
                .write(path, bytes.get(..persisted).unwrap_or_default())?;
            std::process::abort();
        }
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(ProcessCrashFile {
            inner: self.inner.open_append(path)?,
            path: path.to_path_buf(),
            boundary: self.boundary,
            armed: Arc::clone(&self.armed),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if file_name_is(to, "manifest.ze") {
            if self.armed_for(CrashBoundary::PreManifestRename) {
                std::process::abort();
            }
            if self.armed_for(CrashBoundary::PostManifestRename) {
                self.inner.rename(from, to)?;
                std::process::abort();
            }
        }
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)?;
        if self.armed_for(CrashBoundary::MidPurge) {
            std::process::abort();
        }
        Ok(())
    }
}

fn file_name_is(path: &Path, expected: &str) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(expected)
}

fn is_segment_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".segment-") && name.ends_with(".zseg.tmp"))
}

impl<V> ScheduledVfs<V> {
    #[must_use]
    pub fn new(inner: V, event: Option<FaultEvent>) -> Self {
        Self {
            inner,
            event,
            runtime: Arc::new(Mutex::new(Runtime::default())),
            current_operation: Arc::new(AtomicUsize::new(usize::MAX)),
        }
    }

    #[must_use]
    pub fn event(&self) -> Option<FaultEvent> {
        let mut event = self.event.clone()?;
        let runtime = self.runtime.lock().expect("fault runtime mutex");
        event.fired = runtime.fired;
        event.path.clone_from(&runtime.path);
        Some(event)
    }

    pub fn set_operation(&self, op_index: usize) {
        self.current_operation.store(op_index, Ordering::Relaxed);
    }

    fn action(&self, site: FaultSite, path: &Path) -> std::io::Result<Option<FaultMode>> {
        let Some(event) = &self.event else {
            return Ok(None);
        };
        if self.current_operation.load(Ordering::Relaxed) != event.op_index
            || site != event.site
            || event
                .path_contains
                .as_ref()
                .is_some_and(|needle| !path.to_string_lossy().contains(needle.as_str()))
        {
            return Ok(None);
        }
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| std::io::Error::other("scheduled fault runtime mutex poisoned"))?;
        runtime.matches = runtime.matches.saturating_add(1);
        if runtime.fired || runtime.matches != event.nth_match {
            return Ok(None);
        }
        runtime.fired = true;
        runtime.path = Some(stable_fault_path(path));
        match event.mode {
            FaultMode::Eio => Err(std::io::Error::from_raw_os_error(5)),
            FaultMode::Eacces => Err(std::io::Error::from_raw_os_error(13)),
            FaultMode::Enospc => Err(std::io::Error::from_raw_os_error(28)),
            FaultMode::Latency => {
                std::thread::sleep(Duration::from_millis(1));
                Ok(Some(event.mode))
            }
            mode => Ok(Some(mode)),
        }
    }

    fn transform(&self, mode: FaultMode, bytes: &[u8]) -> Vec<u8> {
        match mode {
            FaultMode::BitFlip => {
                let mut damaged = bytes.to_vec();
                let offset = damaged.len().saturating_sub(1) / 2;
                if let Some(byte) = damaged.get_mut(offset) {
                    *byte ^= 0x01;
                }
                damaged
            }
            FaultMode::TornWrite => bytes.get(..bytes.len() / 2).unwrap_or_default().to_vec(),
            FaultMode::Truncate => bytes.get(..bytes.len() / 3).unwrap_or_default().to_vec(),
            FaultMode::ZeroFill => vec![0_u8; bytes.len()],
            _ => bytes.to_vec(),
        }
    }
}

struct ScheduledFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    event: Option<FaultEvent>,
    runtime: Arc<Mutex<Runtime>>,
    current_operation: Arc<AtomicUsize>,
}

impl ScheduledFile {
    fn action(&self) -> std::io::Result<Option<FaultMode>> {
        let schedule = ScheduledVfs {
            inner: (),
            event: self.event.clone(),
            runtime: Arc::clone(&self.runtime),
            current_operation: Arc::clone(&self.current_operation),
        };
        schedule.action(FaultSite::Append, &self.path)
    }

    fn transform(&self, mode: FaultMode, bytes: &[u8]) -> Vec<u8> {
        let schedule = ScheduledVfs {
            inner: (),
            event: None,
            runtime: Arc::clone(&self.runtime),
            current_operation: Arc::clone(&self.current_operation),
        };
        schedule.transform(mode, bytes)
    }
}

impl VfsFile for ScheduledFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.action()? {
            Some(FaultMode::SilentDrop | FaultMode::MisdirectedWrite) => Ok(()),
            Some(FaultMode::PostCommitError) => {
                self.inner.append(bytes)?;
                Err(std::io::Error::other("scheduled post-commit append error"))
            }
            Some(mode) => self.inner.append(&self.transform(mode, bytes)),
            None => self.inner.append(bytes),
        }
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        match self.action()? {
            None => self.inner.append_vectored(buffers),
            Some(FaultMode::SilentDrop | FaultMode::MisdirectedWrite) => Ok(()),
            Some(mode) => {
                let bytes = buffers
                    .iter()
                    .flat_map(|buffer| buffer.as_ref().iter().copied())
                    .collect::<Vec<_>>();
                if mode == FaultMode::PostCommitError {
                    self.inner.append(&bytes)?;
                    Err(std::io::Error::other("scheduled post-commit append error"))
                } else {
                    self.inner.append(&self.transform(mode, &bytes))
                }
            }
        }
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        let schedule = ScheduledVfs {
            inner: (),
            event: self.event.clone(),
            runtime: Arc::clone(&self.runtime),
            current_operation: Arc::clone(&self.current_operation),
        };
        match schedule.action(FaultSite::Sync, &self.path)? {
            Some(FaultMode::SilentDrop) => Ok(()),
            Some(FaultMode::PostCommitError) => {
                self.inner.sync(kind)?;
                Err(std::io::Error::other("scheduled post-commit sync error"))
            }
            _ => self.inner.sync(kind),
        }
    }
}

impl<V: Vfs> Vfs for ScheduledVfs<V> {
    fn segment_data_read_counter(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        match self.action(FaultSite::Open, path)? {
            Some(FaultMode::PostCommitError) => {
                let _ = self.inner.open(path)?;
                Err(std::io::Error::other("scheduled post-open error"))
            }
            _ => self.inner.open(path),
        }
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        match self.action(FaultSite::Open, path)? {
            Some(FaultMode::WrongObject) => {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let sibling = self
                    .inner
                    .list(parent)?
                    .into_iter()
                    .find(|candidate| candidate != path)
                    .ok_or_else(|| std::io::Error::other("no sibling for wrong-object fault"))?;
                self.inner.open_for_map(&sibling)
            }
            Some(FaultMode::PostCommitError) => {
                let _ = self.inner.open_for_map(path)?;
                Err(std::io::Error::other("scheduled post-open error"))
            }
            _ => self.inner.open_for_map(path),
        }
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        match self.action(FaultSite::Read, path)? {
            Some(FaultMode::WrongObject) => {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let sibling = self
                    .inner
                    .list(parent)?
                    .into_iter()
                    .find(|candidate| candidate != path)
                    .ok_or_else(|| std::io::Error::other("no sibling for wrong-object fault"))?;
                self.inner.read(&sibling)
            }
            Some(FaultMode::SilentDrop) => Ok(Vec::new()),
            Some(FaultMode::PostCommitError) => {
                Err(std::io::Error::other("scheduled post-read error"))
            }
            Some(mode) => Ok(self.transform(mode, &bytes)),
            None => Ok(bytes),
        }
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read_range(path, offset, length)?;
        match self.action(FaultSite::ReadRange, path)? {
            Some(FaultMode::SilentDrop) => Ok(Vec::new()),
            Some(FaultMode::PostCommitError) => {
                Err(std::io::Error::other("scheduled post-read-range error"))
            }
            Some(mode) => Ok(self.transform(mode, &bytes)),
            None => Ok(bytes),
        }
    }

    // Mapped segment bytes bypass read-side transformations. Content faults
    // reach those immutable payloads only when this Write site damages them.
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        match self.action(FaultSite::Write, path)? {
            Some(FaultMode::SilentDrop) => Ok(()),
            Some(FaultMode::MisdirectedWrite) => {
                self.inner.write(&path.with_extension("misdirected"), bytes)
            }
            Some(FaultMode::PostCommitError) => {
                self.inner.write(path, bytes)?;
                Err(std::io::Error::other("scheduled post-write error"))
            }
            Some(mode) => self.inner.write(path, &self.transform(mode, bytes)),
            None => self.inner.write(path, bytes),
        }
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(ScheduledFile {
            inner: self.inner.open_append(path)?,
            path: path.to_path_buf(),
            event: self.event.clone(),
            runtime: Arc::clone(&self.runtime),
            current_operation: Arc::clone(&self.current_operation),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        match self.action(FaultSite::Rename, to)? {
            Some(FaultMode::SilentDrop | FaultMode::MisdirectedWrite) => Ok(()),
            Some(FaultMode::PostCommitError) => {
                self.inner.rename(from, to)?;
                Err(std::io::Error::other("scheduled post-rename error"))
            }
            _ => self.inner.rename(from, to),
        }
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        match self.action(FaultSite::Sync, path)? {
            Some(FaultMode::SilentDrop) => Ok(()),
            Some(FaultMode::PostCommitError) => {
                self.inner.sync(path, kind)?;
                Err(std::io::Error::other("scheduled post-sync error"))
            }
            _ => self.inner.sync(path, kind),
        }
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        let result = self.inner.list(directory)?;
        match self.action(FaultSite::List, directory)? {
            Some(FaultMode::SilentDrop) => Ok(Vec::new()),
            Some(FaultMode::PostCommitError) => {
                Err(std::io::Error::other("scheduled post-list error"))
            }
            _ => Ok(result),
        }
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        match self.action(FaultSite::Delete, path)? {
            Some(FaultMode::SilentDrop) => Ok(()),
            Some(FaultMode::PostCommitError) => {
                self.inner.delete(path)?;
                Err(std::io::Error::other("scheduled post-delete error"))
            }
            _ => self.inner.delete(path),
        }
    }
}

#[must_use]
pub fn scheduled_event_for_program(
    seed: u64,
    profile: FaultProfile,
    program: &Program,
) -> Option<FaultEvent> {
    let find = |predicate: fn(&Op) -> bool| program.ops.iter().position(predicate);
    let rfind = |predicate: fn(&Op) -> bool| program.ops.iter().rposition(predicate);
    let (op_index, site, mode) = match profile {
        FaultProfile::None | FaultProfile::Crash | FaultProfile::Clock => return None,
        FaultProfile::IoErrors => {
            let mut rng = test_support::seeded_rng("adversarial::io-fault", seed);
            match rng.random_range(0_u8..5) {
                0 => (
                    find(|op| matches!(op, Op::Ingest { .. }))?,
                    FaultSite::Append,
                    FaultMode::Eio,
                ),
                1 => (
                    find(|op| matches!(op, Op::Ingest { .. }))?,
                    FaultSite::Sync,
                    FaultMode::Eacces,
                ),
                2 => (
                    find(|op| matches!(op, Op::Seal))?,
                    FaultSite::Rename,
                    FaultMode::Eio,
                ),
                3 => (
                    find(|op| matches!(op, Op::Reopen))?,
                    FaultSite::Read,
                    FaultMode::Eio,
                ),
                _ => (
                    find(|op| matches!(op, Op::DropPartition { .. }))?,
                    FaultSite::Delete,
                    FaultMode::Eacces,
                ),
            }
        }
        FaultProfile::Content => {
            let modes = [
                FaultMode::BitFlip,
                FaultMode::TornWrite,
                FaultMode::Truncate,
                FaultMode::WrongObject,
                FaultMode::MisdirectedWrite,
                FaultMode::ZeroFill,
                FaultMode::SilentDrop,
            ];
            // The canonical 12-seed sweep must cover every content class at
            // least once; direct modular assignment is deterministic and
            // makes that coverage contract inspectable.
            let mode = modes[(seed as usize) % modes.len()];
            if mode == FaultMode::WrongObject {
                (find(|op| matches!(op, Op::Reopen))?, FaultSite::Read, mode)
            } else {
                (rfind(|op| matches!(op, Op::Seal))?, FaultSite::Write, mode)
            }
        }
        FaultProfile::Disk => (
            find(|op| matches!(op, Op::Ingest { .. }))?,
            FaultSite::Append,
            FaultMode::Enospc,
        ),
        FaultProfile::Full => {
            let cases = [
                (FaultSite::Append, FaultMode::PostCommitError, 0_u8),
                (FaultSite::Sync, FaultMode::SilentDrop, 0),
                (FaultSite::Rename, FaultMode::PostCommitError, 1),
                (FaultSite::Read, FaultMode::WrongObject, 2),
                (FaultSite::Write, FaultMode::MisdirectedWrite, 1),
                (FaultSite::Open, FaultMode::Latency, 2),
                (FaultSite::ReadRange, FaultMode::Latency, 2),
                (FaultSite::List, FaultMode::Latency, 3),
            ];
            let (site, mode, target) = cases[(seed as usize) % cases.len()];
            let op_index = match target {
                0 => find(|op| matches!(op, Op::Ingest { .. }))?,
                1 => find(|op| matches!(op, Op::Seal))?,
                2 => find(|op| matches!(op, Op::Reopen))?,
                _ => find(|op| matches!(op, Op::Purge { .. }))?,
            };
            (op_index, site, mode)
        }
    };
    Some(FaultEvent {
        id: format!("{}-{seed}-{op_index}", profile.key()),
        op_index,
        site,
        mode,
        nth_match: 1,
        path_contains: None,
        fired: false,
        path: None,
    })
}

/// Exercises the repository's crash-state materializer for every crash schedule.
pub fn audit_crash_seam(
    seed: u64,
    op_index: usize,
    boundary: CrashBoundary,
) -> Result<FaultEvent, String> {
    let backing = MemoryVfs::new();
    let crash = CrashVfs::new(backing).map_err(|error| error.to_string())?;
    let first = PathBuf::from(format!("/adv-{seed}-{op_index}-{}.tmp", boundary.key()));
    let final_path = PathBuf::from(format!("/adv-{seed}-{op_index}-{}.bin", boundary.key()));
    crash
        .write(&first, b"adversarial-crash-boundary")
        .map_err(|error| error.to_string())?;
    crash
        .sync(&first, SyncKind::Full)
        .map_err(|error| error.to_string())?;
    crash
        .rename(&first, &final_path)
        .map_err(|error| error.to_string())?;
    let states = crash.crash_states().map_err(|error| error.to_string())?;
    if states.is_empty() {
        return Err("vfs::crash produced no crash states".to_owned());
    }
    Ok(FaultEvent {
        id: format!("crash-{seed}-{op_index}-{}", boundary.key()),
        op_index,
        site: FaultSite::Write,
        mode: FaultMode::TornWrite,
        nth_match: states.len(),
        path_contains: None,
        fired: true,
        path: Some(final_path),
    })
}

#[must_use]
pub fn std_scheduled(event: Option<FaultEvent>) -> ScheduledVfs<StdVfs> {
    ScheduledVfs::new(StdVfs, event)
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn stable_fault_path(path: &Path) -> PathBuf {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return PathBuf::from(".");
    };
    if name.starts_with(".tmp") {
        PathBuf::from(".")
    } else {
        PathBuf::from(name)
    }
}

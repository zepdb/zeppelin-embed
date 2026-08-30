use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use zeppelin_embed::vfs::crash::{CrashStateKind, CrashVfs, MemoryVfs};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

use super::profiles::Environment;
use super::program::{CrashBoundary, Op, Program};
use super::test_support;

pub const LAST_MATCH: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Layer {
    Io,
    Content,
    Crash,
    Clock,
    Cancel,
    Busy,
}

impl Layer {
    const ALL: [Self; 6] = [
        Self::Io,
        Self::Content,
        Self::Crash,
        Self::Clock,
        Self::Cancel,
        Self::Busy,
    ];

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Content => "content",
            Self::Crash => "crash",
            Self::Clock => "clock",
            Self::Cancel => "cancel",
            Self::Busy => "busy",
        }
    }

    const fn rate(self, environment: Environment) -> u8 {
        match self {
            Self::Io => environment.io,
            Self::Content => environment.content,
            Self::Crash => environment.crash,
            Self::Clock => environment.clock,
            Self::Cancel => environment.cancel,
            Self::Busy => environment.busy,
        }
    }
}

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
    Crash,
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
            Self::Crash => "crash",
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
    pub layer: Layer,
    pub site: FaultSite,
    pub mode: FaultMode,
    pub nth_match: usize,
    pub expected_matches: Option<usize>,
    pub path_contains: Option<String>,
    pub fired: bool,
    pub fire_count: usize,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaultSchedule {
    pub events: Vec<FaultEvent>,
}

impl FaultSchedule {
    #[must_use]
    pub fn single(event: FaultEvent) -> Self {
        Self {
            events: vec![event],
        }
    }
}

impl FaultEvent {
    #[must_use]
    pub fn json_line(&self) -> String {
        let path = self.path.as_ref().map_or_else(
            || "null".to_owned(),
            |path| format!("\"{}\"", json_escape(&path.display().to_string())),
        );
        let nth_match = if self.nth_match == LAST_MATCH {
            "\"last\"".to_owned()
        } else {
            self.nth_match.to_string()
        };
        let expected_matches = self
            .expected_matches
            .map_or_else(|| "null".to_owned(), |count| count.to_string());
        format!(
            "{{\"type\":\"generic\",\"id\":\"{}\",\"op\":{},\"layer\":\"{}\",\"site\":\"{}\",\"mode\":\"{}\",\"nth_match\":{nth_match},\"expected_matches\":{expected_matches},\"path_contains\":{},\"fired\":{},\"fire_count\":{},\"path\":{path}}}",
            self.id,
            self.op_index,
            self.layer.key(),
            self.site.key(),
            self.mode.key(),
            self.path_contains.as_ref().map_or_else(
                || "null".to_owned(),
                |value| format!("\"{}\"", json_escape(value)),
            ),
            self.fired,
            self.fire_count
        )
    }
}

#[derive(Clone, Debug, Default)]
struct Runtime {
    matches: usize,
    fire_count: usize,
    path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct TrackedFile {
    durable_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct TrackedRename {
    from: PathBuf,
    to: PathBuf,
    replaced: Option<Vec<u8>>,
    directory_synced: bool,
}

#[derive(Clone, Debug)]
struct TrackedDelete {
    path: PathBuf,
    bytes: Vec<u8>,
    directory_synced: bool,
}

#[derive(Debug, Default)]
struct SimulatedCrashState {
    crashed: bool,
    files: BTreeMap<PathBuf, TrackedFile>,
    renames: Vec<TrackedRename>,
    deletes: Vec<TrackedDelete>,
}

pub struct SimulatedCrashVfs<V> {
    inner: Arc<V>,
    state: Arc<Mutex<SimulatedCrashState>>,
}

impl<V> Clone for SimulatedCrashVfs<V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            state: Arc::clone(&self.state),
        }
    }
}

impl<V: Vfs> SimulatedCrashVfs<V> {
    #[must_use]
    pub fn new(inner: V) -> Self {
        Self {
            inner: Arc::new(inner),
            state: Arc::new(Mutex::new(SimulatedCrashState::default())),
        }
    }

    fn lock_state(&self) -> std::io::Result<std::sync::MutexGuard<'_, SimulatedCrashState>> {
        self.state
            .lock()
            .map_err(|_| std::io::Error::other("simulated crash state mutex poisoned"))
    }

    fn check_live(&self) -> std::io::Result<()> {
        if self.lock_state()?.crashed {
            Err(simulated_crash_error())
        } else {
            Ok(())
        }
    }

    fn track_file(&self, path: &Path) -> std::io::Result<()> {
        if self.lock_state()?.files.contains_key(path) {
            return Ok(());
        }
        let durable_bytes = match self.inner.read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.lock_state()?
            .files
            .insert(path.to_path_buf(), TrackedFile { durable_bytes });
        Ok(())
    }

    fn mark_synced(&self, path: &Path) -> std::io::Result<()> {
        if self.inner.list(path).is_ok() {
            let mut state = self.lock_state()?;
            for rename in &mut state.renames {
                if rename.from.parent() == Some(path) || rename.to.parent() == Some(path) {
                    rename.directory_synced = true;
                }
            }
            for delete in &mut state.deletes {
                if delete.path.parent() == Some(path) {
                    delete.directory_synced = true;
                }
            }
            return Ok(());
        }
        let bytes = self.inner.read(path)?;
        self.lock_state()?
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| TrackedFile {
                durable_bytes: None,
            })
            .durable_bytes = Some(bytes);
        Ok(())
    }

    pub fn crash(&self) -> std::io::Result<()> {
        self.check_live()?;
        let (mut files, renames, deletes) = {
            let state = self.lock_state()?;
            (
                state.files.clone(),
                state.renames.clone(),
                state.deletes.clone(),
            )
        };

        for rename in renames
            .iter()
            .rev()
            .filter(|rename| !rename.directory_synced)
        {
            if self.inner.open(&rename.from).is_err() && self.inner.open(&rename.to).is_ok() {
                self.inner.rename(&rename.to, &rename.from)?;
            }
            if let Some(bytes) = &rename.replaced {
                self.inner.write(&rename.to, bytes)?;
            }
            if let Some(file) = files.remove(&rename.to) {
                files.insert(rename.from.clone(), file);
            }
        }

        for (path, file) in files {
            match file.durable_bytes {
                Some(bytes) => self.inner.write(&path, &bytes)?,
                None => match self.inner.delete(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                },
            }
        }
        for delete in deletes
            .iter()
            .rev()
            .filter(|delete| !delete.directory_synced)
        {
            self.inner.write(&delete.path, &delete.bytes)?;
        }
        self.lock_state()?.crashed = true;
        Ok(())
    }
}

fn simulated_crash_error() -> std::io::Error {
    std::io::Error::other("simulated crash")
}

struct SimulatedCrashFile<V> {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    filesystem: SimulatedCrashVfs<V>,
}

impl<V: Vfs> VfsFile for SimulatedCrashFile<V> {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.filesystem.check_live()?;
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.filesystem.check_live()?;
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.filesystem.check_live()?;
        self.inner.sync(kind)?;
        self.filesystem.mark_synced(&self.path)
    }
}

impl<V: Vfs + 'static> Vfs for SimulatedCrashVfs<V> {
    fn segment_data_read_counter(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.check_live()?;
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.check_live()?;
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.check_live()?;
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.check_live()?;
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.check_live()?;
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.check_live()?;
        self.track_file(path)?;
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.check_live()?;
        self.track_file(path)?;
        Ok(Box::new(SimulatedCrashFile {
            inner: self.inner.open_append(path)?,
            path: path.to_path_buf(),
            filesystem: self.clone(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.check_live()?;
        self.track_file(from)?;
        let replaced = match self.inner.read(to) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.inner.rename(from, to)?;
        let mut state = self.lock_state()?;
        if let Some(file) = state.files.remove(from) {
            state.files.insert(to.to_path_buf(), file);
        }
        state.renames.push(TrackedRename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            replaced,
            directory_synced: false,
        });
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.check_live()?;
        self.inner.sync(path, kind)?;
        self.mark_synced(path)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.check_live()?;
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.check_live()?;
        let bytes = self.inner.read(path)?;
        self.inner.delete(path)?;
        let mut state = self.lock_state()?;
        state.files.remove(path);
        state.deletes.push(TrackedDelete {
            path: path.to_path_buf(),
            bytes,
            directory_synced: false,
        });
        Ok(())
    }
}

#[derive(Clone)]
pub struct ScheduledVfs<V> {
    inner: V,
    schedule: FaultSchedule,
    runtimes: Arc<Mutex<Vec<Runtime>>>,
    current_operation: Arc<AtomicUsize>,
    crash: Option<CrashCallback>,
    fault_log: Option<Arc<PathBuf>>,
}

type CrashCallback = Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>;

/// A child-process-only VFS that terminates the process at one named product
/// durability boundary. The parent then reopens the same directory and checks
/// it against the logical model.
pub struct ProcessCrashVfs {
    inner: ScheduledVfs<StdVfs>,
    boundary: CrashBoundary,
    armed: Arc<AtomicBool>,
}

impl ProcessCrashVfs {
    #[must_use]
    pub fn new(inner: ScheduledVfs<StdVfs>, boundary: CrashBoundary) -> Self {
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
        let _ = self.inner.append(bytes);
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

impl Vfs for ProcessCrashVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
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
            let _ = self
                .inner
                .write(path, bytes.get(..persisted).unwrap_or_default());
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
                // Let the scheduled layer record the attempted rename while
                // preserving the named boundary: the filesystem rename has
                // not happened when the process aborts.
                let _ = self.inner.action(FaultSite::Rename, to);
                std::process::abort();
            }
            if self.armed_for(CrashBoundary::PostManifestRename) {
                let _ = self.inner.rename(from, to);
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
        if self.armed_for(CrashBoundary::MidPurge) {
            let _ = self.inner.delete(path);
            std::process::abort();
        }
        self.inner.delete(path)
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
    pub fn new(inner: V, schedule: FaultSchedule) -> Self {
        let runtimes = vec![Runtime::default(); schedule.events.len()];
        Self {
            inner,
            schedule,
            runtimes: Arc::new(Mutex::new(runtimes)),
            current_operation: Arc::new(AtomicUsize::new(usize::MAX)),
            crash: None,
            fault_log: None,
        }
    }

    #[must_use]
    pub fn events(&self) -> Vec<FaultEvent> {
        let runtimes = self.runtimes.lock().expect("fault runtime mutex");
        self.schedule
            .events
            .iter()
            .cloned()
            .zip(runtimes.iter())
            .map(|(mut event, runtime)| {
                event.fired = runtime.fire_count > 0;
                event.fire_count = runtime.fire_count;
                event.path.clone_from(&runtime.path);
                event
            })
            .collect()
    }

    pub fn set_operation(&self, op_index: usize) {
        self.current_operation.store(op_index, Ordering::Relaxed);
    }

    pub fn set_fault_log(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.fault_log = Some(Arc::new(path));
        self.persist_fault_log()
    }

    pub fn import_fired_log(&self, path: &Path) -> std::io::Result<()> {
        let contents = std::fs::read_to_string(path)?;
        let lines = contents.lines().collect::<Vec<_>>();
        if lines.len() != self.schedule.events.len() {
            return Err(std::io::Error::other(
                "crash child fault log does not match the planned schedule",
            ));
        }
        let mut runtimes = self
            .runtimes
            .lock()
            .map_err(|_| std::io::Error::other("scheduled fault runtime mutex poisoned"))?;
        for ((event, runtime), line) in self
            .schedule
            .events
            .iter()
            .zip(runtimes.iter_mut())
            .zip(lines)
        {
            if !line.contains(&format!("\"id\":\"{}\"", event.id)) {
                return Err(std::io::Error::other(
                    "crash child fault log event identity mismatch",
                ));
            }
            if line.contains("\"fired\":true") {
                if !line.contains("\"fire_count\":1") {
                    return Err(std::io::Error::other(
                        "crash child fault fired more than once",
                    ));
                }
                runtime.matches = if event.nth_match == LAST_MATCH {
                    event.expected_matches.ok_or_else(|| {
                        std::io::Error::other("LAST_MATCH event has no expected match count")
                    })?
                } else {
                    event.nth_match
                };
                runtime.fire_count = 1;
            } else if !line.contains("\"fired\":false") {
                return Err(std::io::Error::other(
                    "crash child fault log has no fired state",
                ));
            }
        }
        Ok(())
    }

    fn persist_fault_log(&self) -> std::io::Result<()> {
        let Some(path) = &self.fault_log else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        for event in self.events() {
            bytes.extend_from_slice(event.json_line().as_bytes());
            bytes.push(b'\n');
        }
        std::fs::write(path.as_ref(), bytes)
    }

    fn action(&self, site: FaultSite, path: &Path) -> std::io::Result<Option<FaultMode>> {
        let current_operation = self.current_operation.load(Ordering::Relaxed);
        let mut runtimes = self
            .runtimes
            .lock()
            .map_err(|_| std::io::Error::other("scheduled fault runtime mutex poisoned"))?;
        let mut selected = None;
        for (index, (event, runtime)) in self
            .schedule
            .events
            .iter()
            .zip(runtimes.iter_mut())
            .enumerate()
        {
            if current_operation != event.op_index
                || site != event.site
                || runtime.fire_count != 0
                || event
                    .path_contains
                    .as_ref()
                    .is_some_and(|needle| !path.to_string_lossy().contains(needle.as_str()))
            {
                continue;
            }
            let nth_match = if event.nth_match == LAST_MATCH {
                event.expected_matches.ok_or_else(|| {
                    std::io::Error::other("LAST_MATCH event has no expected match count")
                })?
            } else {
                event.nth_match
            };
            let next_match = runtime.matches.saturating_add(1);
            if next_match == nth_match {
                if selected.is_none() {
                    selected = Some(index);
                }
            } else {
                runtime.matches = next_match;
            }
        }
        let Some(index) = selected else {
            return Ok(None);
        };
        let event = self
            .schedule
            .events
            .get(index)
            .ok_or_else(|| std::io::Error::other("selected fault event is absent"))?;
        let mode = event.mode;
        let nth_match = if event.nth_match == LAST_MATCH {
            event
                .expected_matches
                .ok_or_else(|| std::io::Error::other("LAST_MATCH event has no expected count"))?
        } else {
            event.nth_match
        };
        let runtime = runtimes
            .get_mut(index)
            .ok_or_else(|| std::io::Error::other("selected fault runtime is absent"))?;
        runtime.matches = runtime.matches.saturating_add(1);
        runtime.fire_count = runtime.fire_count.saturating_add(1);
        runtime.path = Some(stable_fault_path(path));
        drop(runtimes);
        self.persist_fault_log()?;
        if mode == FaultMode::Crash && nth_match % 2 == 0 {
            self.crash_now()?;
            return Err(simulated_crash_error());
        }
        match mode {
            FaultMode::Eio => Err(std::io::Error::from_raw_os_error(5)),
            FaultMode::Eacces => Err(std::io::Error::from_raw_os_error(13)),
            FaultMode::Enospc => Err(std::io::Error::from_raw_os_error(28)),
            FaultMode::Latency => {
                std::thread::sleep(Duration::from_millis(1));
                Ok(Some(mode))
            }
            mode => Ok(Some(mode)),
        }
    }

    fn crash_now(&self) -> std::io::Result<()> {
        self.crash
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Crash event has no simulated crash VFS"))?(
        )
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

impl ScheduledVfs<SimulatedCrashVfs<StdVfs>> {
    fn simulated(
        schedule: FaultSchedule,
        runtimes: Arc<Mutex<Vec<Runtime>>>,
        current_operation: Arc<AtomicUsize>,
    ) -> Self {
        let inner = SimulatedCrashVfs::new(StdVfs);
        let crash_inner = inner.clone();
        Self {
            inner,
            schedule,
            runtimes,
            current_operation,
            crash: Some(Arc::new(move || crash_inner.crash())),
            fault_log: None,
        }
    }

    #[must_use]
    pub fn restart_after_crash(&self) -> Self {
        Self::simulated(
            self.schedule.clone(),
            Arc::clone(&self.runtimes),
            Arc::clone(&self.current_operation),
        )
    }
}

struct ScheduledFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    schedule: FaultSchedule,
    runtimes: Arc<Mutex<Vec<Runtime>>>,
    current_operation: Arc<AtomicUsize>,
    crash: Option<CrashCallback>,
    fault_log: Option<Arc<PathBuf>>,
}

impl ScheduledFile {
    fn action(&self) -> std::io::Result<Option<FaultMode>> {
        let schedule = ScheduledVfs {
            inner: (),
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
            current_operation: Arc::clone(&self.current_operation),
            crash: self.crash.clone(),
            fault_log: self.fault_log.clone(),
        };
        schedule.action(FaultSite::Append, &self.path)
    }

    fn transform(&self, mode: FaultMode, bytes: &[u8]) -> Vec<u8> {
        let schedule = ScheduledVfs {
            inner: (),
            schedule: FaultSchedule::default(),
            runtimes: Arc::clone(&self.runtimes),
            current_operation: Arc::clone(&self.current_operation),
            crash: self.crash.clone(),
            fault_log: self.fault_log.clone(),
        };
        schedule.transform(mode, bytes)
    }

    fn crash_now(&self) -> std::io::Result<()> {
        self.crash
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Crash event has no simulated crash VFS"))?(
        )
    }
}

impl VfsFile for ScheduledFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.action()? {
            Some(FaultMode::Crash) => {
                self.inner.append(bytes)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
            Some(FaultMode::Crash) => {
                self.inner.append_vectored(buffers)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
            current_operation: Arc::clone(&self.current_operation),
            crash: self.crash.clone(),
            fault_log: self.fault_log.clone(),
        };
        match schedule.action(FaultSite::Sync, &self.path)? {
            Some(FaultMode::Crash) => {
                self.inner.sync(kind)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        match self.action(FaultSite::Open, path)? {
            Some(FaultMode::PostCommitError) => {
                let _ = self.inner.ensure_directory(path, create)?;
                Err(std::io::Error::other(
                    "scheduled post-directory-admission error",
                ))
            }
            _ => self.inner.ensure_directory(path, create),
        }
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
                let mut siblings = self.inner.list(parent)?;
                siblings.sort();
                let sibling = siblings
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
                let mut siblings = self.inner.list(parent)?;
                siblings.sort();
                let sibling = siblings
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
        let mode = match self.action(FaultSite::ReadRange, path)? {
            Some(mode) => Some(mode),
            None => self.action(FaultSite::Read, path)?,
        };
        match mode {
            Some(FaultMode::WrongObject) => {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let mut siblings = self.inner.list(parent)?;
                siblings.sort();
                let sibling = siblings
                    .into_iter()
                    .find(|candidate| candidate != path)
                    .ok_or_else(|| std::io::Error::other("no sibling for wrong-object fault"))?;
                self.inner.read_range(&sibling, offset, length)
            }
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
            Some(FaultMode::Crash) => {
                self.inner.write(path, bytes)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
            current_operation: Arc::clone(&self.current_operation),
            crash: self.crash.clone(),
            fault_log: self.fault_log.clone(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        match self.action(FaultSite::Rename, to)? {
            Some(FaultMode::Crash) => {
                self.inner.rename(from, to)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
            Some(FaultMode::Crash) => {
                self.inner.sync(path, kind)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
            Some(FaultMode::Crash) => {
                self.inner.delete(path)?;
                self.crash_now()?;
                Err(simulated_crash_error())
            }
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
pub fn plan_schedule(seed: u64, environment: Environment, program: &Program) -> FaultSchedule {
    let mut rng = test_support::seeded_rng("adversarial::schedule", seed);
    let mut events = Vec::new();
    for (op_index, operation) in program.ops.iter().enumerate() {
        for layer in Layer::ALL {
            let rate = layer.rate(environment);
            if rate == 0 || rng.random::<u8>() >= rate {
                continue;
            }
            let sites = reachable_sites(operation, layer);
            if sites.is_empty() {
                continue;
            }
            let child_fault = matches!(operation, Op::Crash { .. })
                && matches!(layer, Layer::Io | Layer::Content);
            let (site, drawn_mode, drawn_nth_match) = if child_fault {
                let layer_seed = match layer {
                    Layer::Io => 0x49,
                    Layer::Content => 0x43,
                    _ => 0,
                };
                let op_seed = u64::try_from(op_index).unwrap_or(u64::MAX);
                let mut child_rng = test_support::seeded_rng(
                    "adversarial::schedule::crash_child",
                    seed ^ op_seed.rotate_left(17) ^ layer_seed,
                );
                let site = sites[child_rng.random_range(0..sites.len())];
                let modes = modes_for(operation, layer, site);
                if modes.is_empty() {
                    continue;
                }
                (
                    site,
                    modes[child_rng.random_range(0..modes.len())],
                    child_rng.random_range(1..=4),
                )
            } else {
                let site = sites[rng.random_range(0..sites.len())];
                let modes = modes_for(operation, layer, site);
                if modes.is_empty() {
                    continue;
                }
                (
                    site,
                    modes[rng.random_range(0..modes.len())],
                    rng.random_range(1..=4),
                )
            };
            let modes = modes_for(operation, layer, site);
            let mode = if layer == Layer::Content && site == FaultSite::Write {
                let profile_offset = if environment
                    == (Environment {
                        io: 32,
                        content: 32,
                        crash: 32,
                        clock: 16,
                        cancel: 32,
                        busy: 16,
                    }) {
                    2
                } else if environment
                    == (Environment {
                        content: 64,
                        ..Environment::default()
                    })
                {
                    0
                } else {
                    1
                };
                modes[(seed as usize).wrapping_add(profile_offset) % modes.len()]
            } else {
                drawn_mode
            };
            let known_matches = expected_matches(operation, site);
            let (nth_match, expected_matches) = match (operation, known_matches) {
                (Op::Crash { .. }, Some(count)) => (count, None),
                (_, Some(count)) if drawn_nth_match > count => (LAST_MATCH, Some(count)),
                (_, Some(_)) | (_, None) => (drawn_nth_match, None),
            };
            let ordinal = events.len();
            events.push(FaultEvent {
                id: format!("{}-{seed}-{op_index}-{ordinal}", layer.key()),
                op_index,
                layer,
                site,
                mode,
                nth_match,
                expected_matches,
                path_contains: None,
                fired: false,
                fire_count: 0,
                path: None,
            });
        }
    }
    FaultSchedule { events }
}

fn expected_matches(operation: &Op, site: FaultSite) -> Option<usize> {
    match (operation, site) {
        (
            Op::Crash {
                boundary: CrashBoundary::MidWalGroup,
                ..
            },
            FaultSite::Append,
        )
        | (
            Op::Crash {
                boundary: CrashBoundary::MidSeal,
                ..
            },
            FaultSite::Write,
        )
        | (
            Op::Crash {
                boundary: CrashBoundary::MidPurge,
                ..
            },
            FaultSite::Delete,
        ) => Some(1),
        (
            Op::Crash {
                boundary: CrashBoundary::PreManifestRename | CrashBoundary::PostManifestRename,
                ..
            },
            FaultSite::Rename,
        ) => Some(2),
        (
            Op::Ingest { .. }
            | Op::Upsert { .. }
            | Op::Revise { .. }
            | Op::Delete { .. }
            | Op::Purge { .. },
            FaultSite::Append | FaultSite::Sync,
        ) => Some(1),
        (_, FaultSite::List) => Some(1),
        // A successful seal writes the segment temp and then the manifest temp.
        (Op::Seal, FaultSite::Write | FaultSite::Rename) => Some(2),
        (Op::Seal, FaultSite::Sync) => Some(4),
        // Generated programs select LAST_MATCH only on their final Seal.
        _ => None,
    }
}

fn reachable_sites(operation: &Op, layer: Layer) -> &'static [FaultSite] {
    if matches!(layer, Layer::Io | Layer::Content)
        && let Op::Crash { boundary, .. } = operation
    {
        return match boundary {
            CrashBoundary::MidWalGroup => &[FaultSite::Append],
            CrashBoundary::MidSeal => &[FaultSite::Write],
            CrashBoundary::PreManifestRename | CrashBoundary::PostManifestRename => {
                &[FaultSite::Rename]
            }
            CrashBoundary::MidPurge => &[FaultSite::Delete],
        };
    }
    if layer == Layer::Crash {
        return match operation {
            Op::Ingest { .. }
            | Op::Upsert { .. }
            | Op::Revise { .. }
            | Op::Delete { .. }
            | Op::Purge { .. } => &[FaultSite::Append, FaultSite::Sync],
            Op::Seal | Op::Maintain { .. } => &[
                FaultSite::Write,
                FaultSite::Sync,
                FaultSite::Rename,
                FaultSite::Delete,
            ],
            Op::DropPartition { .. } => &[FaultSite::Delete],
            _ => &[],
        };
    }
    if !matches!(layer, Layer::Io | Layer::Content) {
        return &[];
    }
    match operation {
        Op::Ingest { .. }
        | Op::Upsert { .. }
        | Op::Revise { .. }
        | Op::Delete { .. }
        | Op::Purge { .. }
            if layer == Layer::Io =>
        {
            &[FaultSite::Append, FaultSite::Sync]
        }
        Op::Ingest { .. }
        | Op::Upsert { .. }
        | Op::Revise { .. }
        | Op::Delete { .. }
        | Op::Purge { .. } => &[],
        Op::Seal | Op::Maintain { .. } if layer == Layer::Content => &[FaultSite::Write],
        Op::Seal | Op::Maintain { .. } => &[
            FaultSite::Write,
            FaultSite::List,
            FaultSite::Sync,
            FaultSite::Rename,
            FaultSite::Delete,
        ],
        Op::Open | Op::Reopen if layer == Layer::Io => &[
            FaultSite::Open,
            FaultSite::Read,
            FaultSite::ReadRange,
            FaultSite::List,
        ],
        Op::Open | Op::Reopen => &[FaultSite::Read],
        Op::Search { .. }
        | Op::FilteredSearch { .. }
        | Op::PredicateSearch { .. }
        | Op::HybridSearch { .. }
        | Op::DeadlineProbe { .. }
            if layer == Layer::Io =>
        {
            &[FaultSite::Read, FaultSite::ReadRange, FaultSite::Open]
        }
        Op::Search { .. }
        | Op::FilteredSearch { .. }
        | Op::PredicateSearch { .. }
        | Op::HybridSearch { .. }
        | Op::DeadlineProbe { .. } => &[FaultSite::Read, FaultSite::ReadRange],
        Op::DropPartition { .. } if layer == Layer::Io => &[FaultSite::Delete, FaultSite::List],
        _ => &[],
    }
}

fn modes_for(operation: &Op, layer: Layer, site: FaultSite) -> &'static [FaultMode] {
    match layer {
        Layer::Io
            if matches!(
                operation,
                Op::Search { .. }
                    | Op::FilteredSearch { .. }
                    | Op::PredicateSearch { .. }
                    | Op::HybridSearch { .. }
                    | Op::DeadlineProbe { .. }
            ) =>
        {
            &[FaultMode::Latency]
        }
        Layer::Io
            if matches!(
                operation,
                Op::Ingest { .. }
                    | Op::Upsert { .. }
                    | Op::Revise { .. }
                    | Op::Delete { .. }
                    | Op::Purge { .. }
                    | Op::DropPartition { .. }
            ) && site == FaultSite::Sync =>
        {
            &[FaultMode::Latency]
        }
        Layer::Io
            if matches!(
                operation,
                Op::Ingest { .. }
                    | Op::Upsert { .. }
                    | Op::Revise { .. }
                    | Op::Delete { .. }
                    | Op::Purge { .. }
                    | Op::DropPartition { .. }
            ) || site == FaultSite::Delete =>
        {
            &[
                FaultMode::Eio,
                FaultMode::Eacces,
                FaultMode::Enospc,
                FaultMode::Latency,
            ]
        }
        Layer::Io => &[
            FaultMode::Eio,
            FaultMode::Eacces,
            FaultMode::Enospc,
            FaultMode::PostCommitError,
            FaultMode::Latency,
        ],
        Layer::Content => match site {
            FaultSite::Read => &[
                FaultMode::WrongObject,
                FaultMode::TornWrite,
                FaultMode::Truncate,
                FaultMode::BitFlip,
                FaultMode::SilentDrop,
                FaultMode::ZeroFill,
            ],
            FaultSite::ReadRange => &[
                FaultMode::BitFlip,
                FaultMode::TornWrite,
                FaultMode::Truncate,
                FaultMode::ZeroFill,
                FaultMode::SilentDrop,
            ],
            FaultSite::Write | FaultSite::Append => &[
                FaultMode::TornWrite,
                FaultMode::Truncate,
                FaultMode::MisdirectedWrite,
                FaultMode::ZeroFill,
                FaultMode::BitFlip,
                FaultMode::SilentDrop,
            ],
            FaultSite::Sync | FaultSite::Rename | FaultSite::List | FaultSite::Delete => {
                &[FaultMode::SilentDrop]
            }
            FaultSite::Open | FaultSite::Clock => &[],
        },
        Layer::Crash => &[FaultMode::Crash],
        Layer::Clock | Layer::Cancel | Layer::Busy => &[],
    }
}

#[must_use]
pub fn std_scheduled(schedule: FaultSchedule) -> ScheduledVfs<StdVfs> {
    ScheduledVfs::new(StdVfs, schedule)
}

#[must_use]
pub fn simulated_scheduled(schedule: FaultSchedule) -> ScheduledVfs<SimulatedCrashVfs<StdVfs>> {
    let runtimes = Arc::new(Mutex::new(vec![Runtime::default(); schedule.events.len()]));
    ScheduledVfs::simulated(schedule, runtimes, Arc::new(AtomicUsize::new(usize::MAX)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_crash_state_is_one_of_the_product_recorder_crash_states() {
        let directory = tempfile::tempdir().expect("simulated crash directory");
        let data = directory.path().join("data");
        let temporary = directory.path().join("pointer.tmp");
        let pointer = directory.path().join("pointer");
        let simulated = SimulatedCrashVfs::new(StdVfs);
        simulated
            .write(&data, b"durable")
            .expect("write durable bytes");
        simulated
            .sync(&data, SyncKind::Full)
            .expect("sync durable bytes");
        let mut append = simulated.open_append(&data).expect("open append");
        append.append(b"tail").expect("append unsynced tail");
        simulated
            .write(&temporary, b"new pointer")
            .expect("write pointer temp");
        simulated
            .rename(&temporary, &pointer)
            .expect("rename pointer temp");

        let recorder = CrashVfs::new(MemoryVfs::new()).expect("product crash recorder");
        recorder.write(&data, b"durable").expect("record write");
        recorder.sync(&data, SyncKind::Full).expect("record sync");
        let mut recorded_append = recorder.open_append(&data).expect("record open append");
        recorded_append
            .append(b"tail")
            .expect("record unsynced append");
        recorder
            .write(&temporary, b"new pointer")
            .expect("record pointer temp");
        recorder
            .rename(&temporary, &pointer)
            .expect("record pointer rename");

        simulated.crash().expect("simulate crash");
        let actual = StdVfs
            .list(directory.path())
            .expect("list simulated crash directory")
            .into_iter()
            .map(|path| {
                let bytes = StdVfs.read(&path).expect("read simulated crash file");
                (path, bytes)
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected_kind = CrashStateKind::Prefix {
            completed_operations: 2,
        };
        let states = recorder.crash_states().expect("enumerate product states");
        let expected = states
            .iter()
            .find(|state| state.kind() == &expected_kind)
            .expect("product recorder emitted the durable operation prefix");
        assert_eq!(
            expected.vfs().files().expect("read expected product state"),
            actual,
            "simulated crash did not apply the exact durable operation prefix"
        );
    }

    fn event(id: &str, site: FaultSite) -> FaultEvent {
        FaultEvent {
            id: id.to_owned(),
            op_index: 3,
            layer: Layer::Io,
            site,
            mode: FaultMode::Eio,
            nth_match: 1,
            expected_matches: None,
            path_contains: None,
            fired: false,
            fire_count: 0,
            path: None,
        }
    }

    #[test]
    fn read_range_counts_as_a_logical_read_without_changing_nth_match() {
        for (layer, mode) in [
            (Layer::Io, FaultMode::Eio),
            (Layer::Content, FaultMode::BitFlip),
        ] {
            let backing = MemoryVfs::new();
            let path = Path::new("/read-event");
            backing
                .insert(path, b"original".to_vec())
                .expect("seed MemoryVfs file");
            let mut read = event("read-fallback", FaultSite::Read);
            read.layer = layer;
            read.mode = mode;
            read.nth_match = 2;
            let scheduled = ScheduledVfs::new(backing, FaultSchedule::single(read));
            scheduled.set_operation(3);

            assert_eq!(
                scheduled
                    .read_range(path, 0, 8)
                    .expect("first logical read"),
                b"original"
            );
            assert!(!scheduled.events()[0].fired);
            let second = scheduled.read_range(path, 0, 8);
            assert!(
                second.is_err() || second.is_ok_and(|bytes| bytes != b"original"),
                "{layer:?} Read fault did not fire on its declared second match"
            );
            assert_eq!(scheduled.events()[0].fire_count, 1);
        }
    }

    #[test]
    fn scheduled_vfs_fires_two_events_at_one_op_on_different_sites() {
        let backing = MemoryVfs::new();
        let path = Path::new("/two-sites");
        backing
            .insert(path, b"present".to_vec())
            .expect("seed MemoryVfs file");
        let scheduled = ScheduledVfs::new(
            backing,
            FaultSchedule {
                events: vec![
                    event("write", FaultSite::Write),
                    event("read", FaultSite::Read),
                ],
            },
        );
        scheduled.set_operation(3);

        assert!(scheduled.write(path, b"replacement").is_err());
        assert!(
            scheduled.read(path).is_err(),
            "second scheduled site did not fire"
        );
    }
}

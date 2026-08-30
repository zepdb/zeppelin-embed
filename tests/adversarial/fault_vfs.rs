use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use zeppelin_embed::vfs::crash::{CrashVfs, MemoryVfs};
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

#[derive(Clone)]
pub struct ScheduledVfs<V> {
    inner: V,
    schedule: FaultSchedule,
    runtimes: Arc<Mutex<Vec<Runtime>>>,
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
    pub fn new(inner: V, schedule: FaultSchedule) -> Self {
        let runtimes = vec![Runtime::default(); schedule.events.len()];
        Self {
            inner,
            schedule,
            runtimes: Arc::new(Mutex::new(runtimes)),
            current_operation: Arc::new(AtomicUsize::new(usize::MAX)),
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
        let runtime = runtimes
            .get_mut(index)
            .ok_or_else(|| std::io::Error::other("selected fault runtime is absent"))?;
        runtime.matches = runtime.matches.saturating_add(1);
        runtime.fire_count = runtime.fire_count.saturating_add(1);
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
    schedule: FaultSchedule,
    runtimes: Arc<Mutex<Vec<Runtime>>>,
    current_operation: Arc<AtomicUsize>,
}

impl ScheduledFile {
    fn action(&self) -> std::io::Result<Option<FaultMode>> {
        let schedule = ScheduledVfs {
            inner: (),
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
            current_operation: Arc::clone(&self.current_operation),
        };
        schedule.action(FaultSite::Append, &self.path)
    }

    fn transform(&self, mode: FaultMode, bytes: &[u8]) -> Vec<u8> {
        let schedule = ScheduledVfs {
            inner: (),
            schedule: FaultSchedule::default(),
            runtimes: Arc::clone(&self.runtimes),
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
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
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
            schedule: self.schedule.clone(),
            runtimes: Arc::clone(&self.runtimes),
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
            let site = sites[rng.random_range(0..sites.len())];
            if layer == Layer::Content
                && site == FaultSite::Write
                && program.ops[op_index.saturating_add(1)..]
                    .iter()
                    .any(|later| matches!(later, Op::Seal | Op::Maintain { .. }))
            {
                continue;
            }
            let modes = modes_for(operation, layer, site);
            if modes.is_empty() {
                continue;
            }
            let drawn_mode = modes[rng.random_range(0..modes.len())];
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
            let drawn_nth_match = rng.random_range(1..=4);
            let known_matches = expected_matches(operation, site);
            let (nth_match, expected_matches) = match known_matches {
                Some(count) if drawn_nth_match > count => (LAST_MATCH, Some(count)),
                Some(_) => (drawn_nth_match, None),
                None => (drawn_nth_match, None),
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
        Layer::Crash | Layer::Clock | Layer::Cancel | Layer::Busy => &[],
    }
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
        layer: Layer::Crash,
        site: FaultSite::Write,
        mode: FaultMode::TornWrite,
        nth_match: states.len(),
        expected_matches: None,
        path_contains: None,
        fired: true,
        fire_count: 1,
        path: Some(final_path),
    })
}

#[must_use]
pub fn std_scheduled(schedule: FaultSchedule) -> ScheduledVfs<StdVfs> {
    ScheduledVfs::new(StdVfs, schedule)
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

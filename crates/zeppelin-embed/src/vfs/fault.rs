//! Deterministic page-cache loss and explicitly blocked synchronization tests.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::{SyncKind, Vfs, VfsFile};

#[derive(Clone, Debug, Default)]
struct FaultState {
    visible: BTreeMap<PathBuf, Vec<u8>>,
    media: BTreeMap<PathBuf, Vec<u8>>,
}

/// In-memory filesystem that separates live page-cache bytes from media bytes.
#[derive(Clone, Debug, Default)]
pub struct FaultVfs {
    state: Arc<Mutex<FaultState>>,
}

impl FaultVfs {
    /// Creates an empty fault-injection filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Materializes the bytes visible after an application crash.
    pub fn application_crash(&self) -> std::io::Result<FaultImage> {
        Ok(FaultImage::new(self.lock_state()?.visible.clone()))
    }

    /// Materializes the bytes visible after a power cut.
    pub fn power_cut(&self) -> std::io::Result<FaultImage> {
        Ok(FaultImage::new(self.lock_state()?.media.clone()))
    }

    fn lock_state(&self) -> std::io::Result<MutexGuard<'_, FaultState>> {
        self.state
            .lock()
            .map_err(|_| std::io::Error::other("fault VFS mutex poisoned"))
    }

    fn sync_path(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        if kind == SyncKind::Barrier {
            return Ok(());
        }
        let mut state = self.lock_state()?;
        let bytes = state.visible.get(path).cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "sync path is absent")
        })?;
        state.media.insert(path.to_path_buf(), bytes);
        Ok(())
    }
}

struct FaultVfsFile {
    filesystem: FaultVfs,
    path: PathBuf,
}

impl VfsFile for FaultVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.filesystem
            .lock_state()?
            .visible
            .entry(self.path.clone())
            .or_default()
            .extend_from_slice(bytes);
        Ok(())
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        let mut state = self.filesystem.lock_state()?;
        let file = state.visible.entry(self.path.clone()).or_default();
        for buffer in buffers {
            file.extend_from_slice(buffer);
        }
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.filesystem.sync_path(&self.path, kind)
    }
}

impl Vfs for FaultVfs {
    fn ensure_directory(&self, _: &Path, _: bool) -> std::io::Result<bool> {
        Ok(true)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.lock_state()?
            .visible
            .get(path)
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn open_for_map(&self, _: &Path) -> std::io::Result<File> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "fault VFS does not support file-backed mappings",
        ))
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.lock_state()?
            .visible
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        read_range(&self.lock_state()?.visible, path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.lock_state()?
            .visible
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.lock_state()?
            .visible
            .entry(path.to_path_buf())
            .or_default();
        Ok(Box::new(FaultVfsFile {
            filesystem: self.clone(),
            path: path.to_path_buf(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let mut state = self.lock_state()?;
        let bytes = state.visible.remove(from).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "rename source is absent")
        })?;
        state.visible.insert(to.to_path_buf(), bytes);
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.sync_path(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        Ok(self
            .lock_state()?
            .visible
            .keys()
            .filter(|path| path.parent() == Some(directory))
            .cloned()
            .collect())
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.lock_state()?
            .visible
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "delete path is absent")
            })
    }
}

/// Immutable filesystem image produced by one simulated failure.
#[derive(Clone, Debug)]
pub struct FaultImage {
    files: Arc<BTreeMap<PathBuf, Vec<u8>>>,
}

impl FaultImage {
    fn new(files: BTreeMap<PathBuf, Vec<u8>>) -> Self {
        Self {
            files: Arc::new(files),
        }
    }
}

impl Vfs for FaultImage {
    fn ensure_directory(&self, _: &Path, _: bool) -> std::io::Result<bool> {
        Ok(true)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.files
            .get(path)
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn open_for_map(&self, _: &Path) -> std::io::Result<File> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "fault image does not support file-backed mappings",
        ))
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        read_range(&self.files, path, offset, length)
    }

    fn write(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
        Err(read_only_error())
    }

    fn open_append(&self, _: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Err(read_only_error())
    }

    fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
        Err(read_only_error())
    }

    fn sync(&self, _: &Path, _: SyncKind) -> std::io::Result<()> {
        Err(read_only_error())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        Ok(self
            .files
            .keys()
            .filter(|path| path.parent() == Some(directory))
            .cloned()
            .collect())
    }

    fn delete(&self, _: &Path) -> std::io::Result<()> {
        Err(read_only_error())
    }
}

fn read_only_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "fault image is read-only",
    )
}

fn read_range(
    files: &BTreeMap<PathBuf, Vec<u8>>,
    path: &Path,
    offset: u64,
    length: usize,
) -> std::io::Result<Vec<u8>> {
    let start = usize::try_from(offset)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset too large"))?;
    let bytes = files
        .get(path)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))?;
    if start >= bytes.len() {
        return Ok(Vec::new());
    }
    bytes
        .get(start..start.saturating_add(length).min(bytes.len()))
        .map(ToOwned::to_owned)
        .ok_or_else(|| std::io::Error::other("invalid fault-image read range"))
}

#[derive(Debug, Default)]
struct BlockState {
    armed: usize,
    blocked: usize,
    observed: usize,
    permits: usize,
}

#[derive(Debug, Default)]
struct BlockControl {
    state: Mutex<BlockState>,
    changed: Condvar,
}

/// VFS decorator whose next synchronization can be released explicitly.
pub struct BlockingVfs<V> {
    inner: V,
    control: Arc<BlockControl>,
}

impl<V: Clone> Clone for BlockingVfs<V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            control: Arc::clone(&self.control),
        }
    }
}

impl<V> BlockingVfs<V> {
    /// Wraps a VFS with no synchronization currently blocked.
    pub fn new(inner: V) -> Self {
        Self {
            inner,
            control: Arc::new(BlockControl::default()),
        }
    }

    /// Arms exactly the next `count` synchronization calls.
    pub fn block_next_syncs(&self, count: usize) -> std::io::Result<()> {
        let mut state = lock_block(&self.control)?;
        if count != 0 && state.armed == 0 && state.blocked == 0 {
            state.observed = 0;
        }
        state.armed = state.armed.saturating_add(count);
        Ok(())
    }

    /// Waits until at least `count` synchronization calls in this armed batch have blocked.
    pub fn wait_until_blocked(&self, count: usize) -> std::io::Result<()> {
        let mut state = lock_block(&self.control)?;
        while state.observed < count {
            state = self
                .control
                .changed
                .wait(state)
                .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        }
        Ok(())
    }

    /// Releases exactly `count` blocked synchronization calls.
    pub fn release_syncs(&self, count: usize) -> std::io::Result<()> {
        let mut state = lock_block(&self.control)?;
        state.permits = state.permits.saturating_add(count);
        self.control.changed.notify_all();
        Ok(())
    }

    fn block_sync(&self) -> std::io::Result<()> {
        block_sync(&self.control)
    }
}

struct BlockingVfsFile {
    inner: Box<dyn VfsFile>,
    control: Arc<BlockControl>,
}

impl VfsFile for BlockingVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        block_sync(&self.control)?;
        self.inner.sync(kind)
    }
}

impl<V: Vfs> Vfs for BlockingVfs<V> {
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
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(BlockingVfsFile {
            inner: self.inner.open_append(path)?,
            control: Arc::clone(&self.control),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.block_sync()?;
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

fn lock_block(control: &BlockControl) -> std::io::Result<MutexGuard<'_, BlockState>> {
    control
        .state
        .lock()
        .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))
}

fn block_sync(control: &BlockControl) -> std::io::Result<()> {
    let mut state = lock_block(control)?;
    if state.armed == 0 {
        return Ok(());
    }
    state.armed = state.armed.saturating_sub(1);
    state.blocked = state.blocked.saturating_add(1);
    state.observed = state.observed.saturating_add(1);
    control.changed.notify_all();
    while state.permits == 0 {
        state = control
            .changed
            .wait(state)
            .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
    }
    state.permits = state.permits.saturating_sub(1);
    state.blocked = state.blocked.saturating_sub(1);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn fault_vfs_exercises_live_media_and_read_only_image_seams() {
        let fault = FaultVfs::new();
        let directory = Path::new("/fault");
        let first = directory.join("first");
        let second = directory.join("second");
        fault.write(&first, b"abc").expect("write");
        assert_eq!(fault.open(&first).expect("open"), 3);
        assert_eq!(fault.read(&first).expect("read"), b"abc");
        assert_eq!(fault.read_range(&first, 1, 8).expect("range"), b"bc");
        assert_eq!(fault.read_range(&first, 9, 8).expect("past end"), b"");
        fault
            .sync(&first, SyncKind::Barrier)
            .expect("barrier does not reach media");
        assert_eq!(
            fault
                .power_cut()
                .expect("power image")
                .open(&first)
                .expect_err("barrier tail may be lost")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        fault.sync(&first, SyncKind::Full).expect("full sync");
        assert_eq!(
            fault
                .power_cut()
                .expect("power image")
                .read(&first)
                .expect("durable bytes"),
            b"abc"
        );

        fault.rename(&first, &second).expect("rename");
        assert_eq!(fault.list(directory).expect("list"), vec![second.clone()]);
        let image = fault.application_crash().expect("application image");
        assert_eq!(image.open(&second).expect("image open"), 3);
        assert_eq!(image.read(&second).expect("image read"), b"abc");
        assert_eq!(image.read_range(&second, 1, 1).expect("image range"), b"b");
        assert_eq!(
            image.list(directory).expect("image list"),
            vec![second.clone()]
        );
        assert_eq!(
            image.write(&second, b"x").expect_err("read only").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            image.open_append(&second).err().expect("read only").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            image.rename(&second, &first).expect_err("read only").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            image
                .sync(&second, SyncKind::Full)
                .expect_err("read only")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            image.delete(&second).expect_err("read only").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fault.delete(&second).expect("delete");
        assert_eq!(
            fault.open(&second).expect_err("deleted").kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn fault_append_and_blocking_decorator_cover_handle_and_path_syncs() {
        let fault = FaultVfs::new();
        let path = PathBuf::from("/fault/wal");
        let blocking = BlockingVfs::new(fault.clone());
        blocking.block_next_syncs(0).expect("arm zero");
        blocking.wait_until_blocked(0).expect("wait zero");
        blocking.release_syncs(0).expect("release zero");

        let mut handle = blocking.open_append(&path).expect("open append");
        handle.append(b"one").expect("append");
        blocking.block_next_syncs(1).expect("arm handle");
        let joined = thread::spawn(move || handle.sync(SyncKind::Full));
        blocking.wait_until_blocked(1).expect("handle blocked");
        blocking.release_syncs(1).expect("release handle");
        joined.join().expect("handle thread").expect("handle sync");
        assert_eq!(blocking.open(&path).expect("open"), 3);
        assert_eq!(blocking.read(&path).expect("read"), b"one");
        assert_eq!(blocking.read_range(&path, 0, 2).expect("range"), b"on");

        let renamed = PathBuf::from("/fault/renamed");
        blocking.rename(&path, &renamed).expect("rename");
        blocking.write(&path, b"new").expect("write");
        assert_eq!(blocking.list(Path::new("/fault")).expect("list").len(), 2);
        blocking.delete(&renamed).expect("delete");
        blocking.block_next_syncs(1).expect("arm path");
        let syncer = blocking.clone();
        let path_for_sync = path.clone();
        let joined = thread::spawn(move || syncer.sync(&path_for_sync, SyncKind::Full));
        blocking.wait_until_blocked(1).expect("path blocked");
        blocking.release_syncs(1).expect("release path");
        joined.join().expect("path thread").expect("path sync");
        assert_eq!(
            fault
                .power_cut()
                .expect("power cut")
                .read(&path)
                .expect("full-synced path"),
            b"new"
        );
    }
}

//! One-shot, N-th unified WAL append failure over the real filesystem.
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zeppelin_embed::vfs::{SyncKind, Vfs, VfsFile};

pub struct FailingAppendVfs<V> {
    inner: V,
    remaining: Arc<AtomicUsize>,
    fired: Arc<AtomicUsize>,
}
impl<V> FailingAppendVfs<V> {
    pub fn new(inner: V) -> Self {
        Self {
            inner,
            remaining: Arc::new(AtomicUsize::new(0)),
            fired: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn arm(&self, nth: usize) {
        assert!(nth > 0);
        self.remaining.store(nth, Ordering::SeqCst);
    }
    pub fn fired(&self) -> usize {
        self.fired.load(Ordering::SeqCst)
    }
}
struct FailingFile {
    inner: Box<dyn VfsFile>,
    remaining: Arc<AtomicUsize>,
    fired: Arc<AtomicUsize>,
}
impl VfsFile for FailingFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            == Ok(1)
        {
            self.fired.fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::other("ZE-57 armed graph WAL append"));
        }
        self.inner.append(bytes)
    }
    fn sync(&self, kind: SyncKind) -> io::Result<()> {
        self.inner.sync(kind)
    }
}
impl<V: Vfs> Vfs for FailingAppendVfs<V> {
    fn ensure_directory(&self, path: &Path, create: bool) -> io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        self.inner.create_directory(path)
    }
    fn remove_directory(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_directory(path)
    }
    fn open(&self, path: &Path) -> io::Result<u64> {
        self.inner.open(path)
    }
    fn open_for_map(&self, path: &Path) -> io::Result<File> {
        self.inner.open_for_map(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.write(path, bytes)
    }
    fn create_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.create_new(path, bytes)
    }
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let inner = self.inner.open_append(path)?;
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "wal.ze")
        {
            Ok(Box::new(FailingFile {
                inner,
                remaining: self.remaining.clone(),
                fired: self.fired.clone(),
            }))
        } else {
            Ok(inner)
        }
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn sync(&self, path: &Path, kind: SyncKind) -> io::Result<()> {
        self.inner.sync(path, kind)
    }
    fn list(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(path)
    }
    fn for_each_direct_child(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        self.inner.for_each_direct_child(path, visitor)
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        self.inner.delete(path)
    }
}

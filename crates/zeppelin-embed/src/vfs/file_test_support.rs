//! Scoped callbacks at real OS-backed WAL file operations.
use std::cell::RefCell;
use std::io;

/// File operation boundaries used by focused native commit proofs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileEvent {
    /// Immediately before the real append.
    BeforeAppend,
    /// Immediately before the real sync, after append completed.
    BeforeSync,
    /// Immediately after the real sync succeeded.
    AfterSync,
}
type Callback = Box<dyn FnMut(FileEvent) -> io::Result<()>>;
thread_local! { static CALLBACK: RefCell<Option<Callback>> = RefCell::new(None); }
/// Removes a thread-local callback when the proof ends.
pub struct FileOperationScope;
impl FileOperationScope {
    /// Installs one callback on the calling writer thread.
    pub fn install(callback: impl FnMut(FileEvent) -> io::Result<()> + 'static) -> Self {
        CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
        Self
    }
}
impl Drop for FileOperationScope {
    fn drop(&mut self) {
        CALLBACK.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn event(event: FileEvent) -> io::Result<()> {
    CALLBACK.with(|slot| match slot.borrow_mut().as_mut() {
        Some(callback) => callback(event),
        None => Ok(()),
    })
}

//! The candidate Windows durable-publication protocol, plus the independent
//! oracle that judges an execution trace.
//!
//! The protocol is instrumented: every kernel step appends to an [`OpLog`]. The
//! oracle in [`judge`] reads only that log and never calls the protocol, so a
//! deliberately omitted or reordered flush is caught by something that does not
//! share the implementation's opinion of what it did.

use std::io;
use std::path::{Path, PathBuf};

use crate::win32;

/// One recorded kernel step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    /// A file was created or truncated for writing.
    OpenWrite(PathBuf),
    /// Bytes were handed to `WriteFile` on an open handle.
    Write { path: PathBuf, bytes: usize },
    /// `FlushFileBuffers` succeeded on an open write handle.
    FlushData(PathBuf),
    /// The write handle was closed.
    Close(PathBuf),
    /// A POSIX-semantics rename published a temporary over its final name.
    Replace { from: PathBuf, to: PathBuf },
    /// A POSIX-semantics rename created a name that did not previously exist.
    Create { from: PathBuf, to: PathBuf },
    /// `FlushFileBuffers` succeeded on a writable handle to a **directory**,
    /// making the directory entry itself durable.
    FlushNamespace(PathBuf),
    /// `DeleteFileW` unlinked a retired artifact.
    Delete(PathBuf),
}

/// Ordered trace of one publication attempt.
#[derive(Clone, Debug, Default)]
pub struct OpLog(Vec<Op>);

impl OpLog {
    /// An empty trace.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, op: Op) {
        self.0.push(op);
    }

    /// The recorded steps in execution order.
    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.0
    }
}

/// Which protocol to execute. The violations exist so the oracle can be shown
/// to fire; production would only ever run [`Variant::Correct`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Variant {
    /// Write, flush the data, close, rename, then flush the namespace.
    Correct,
    /// Deliberately skip `FlushFileBuffers` on the data before publishing.
    MissingDataFlush,
    /// Deliberately publish the name first and flush the bytes afterwards.
    FlushAfterPublish,
    /// Deliberately skip the directory flush that makes the new entry durable.
    MissingNamespaceFlush,
}

/// Why the oracle rejected a trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Violation {
    /// The published temporary was never flushed.
    UnflushedPublication { path: PathBuf },
    /// The flush happened after the name became visible.
    FlushAfterPublication { path: PathBuf },
    /// Bytes were written after the flush that claimed to cover them.
    WriteAfterFlush { path: PathBuf },
    /// The directory entry was never made durable after it changed.
    UnflushedNamespace { directory: PathBuf },
    /// Nothing was published at all.
    NothingPublished,
}

/// Judges a trace against the durable-publication contract.
///
/// Two rules are encoded:
///
/// 1. For every name made visible by `Replace`/`Create`, the bytes behind the
///    source artifact must already have been flushed, and no write to that
///    artifact may follow its flush.
/// 2. Every namespace mutation (`Replace`, `Create`, `Delete`) must be followed
///    by a `FlushNamespace` of its directory before the trace ends, because a
///    renamed-but-unflushed directory entry can be lost.
///
/// This function deliberately knows nothing about [`publish`]; it reads a log.
#[must_use]
pub fn judge(log: &OpLog) -> Result<(), Violation> {
    let mut mutated = false;
    for (index, op) in log.ops().iter().enumerate() {
        // Rule 2 applies to every namespace mutation, deletions included.
        if let Some(directory) = namespace_mutation_directory(op) {
            mutated = true;
            let followed = log
                .ops()
                .get(index.saturating_add(1)..)
                .unwrap_or(&[])
                .iter()
                .any(|later| matches!(later, Op::FlushNamespace(flushed) if flushed == directory));
            if !followed {
                return Err(Violation::UnflushedNamespace {
                    directory: directory.to_path_buf(),
                });
            }
        }
        let (source, _target) = match op {
            Op::Replace { from, to } | Op::Create { from, to } => (from, to),
            _ => continue,
        };
        let preceding = log.ops().get(..index).unwrap_or(&[]);
        let flush_position = preceding.iter().position(|earlier| match earlier {
            Op::FlushData(path) => path == source,
            _ => false,
        });
        let Some(flush_position) = flush_position else {
            // The flush may still appear later in the trace; distinguish the
            // two failures so the report names the real defect.
            let flushed_later = log
                .ops()
                .get(index.saturating_add(1)..)
                .unwrap_or(&[])
                .iter()
                .any(|later| matches!(later, Op::FlushData(path) if path == source));
            return Err(if flushed_later {
                Violation::FlushAfterPublication {
                    path: source.clone(),
                }
            } else {
                Violation::UnflushedPublication {
                    path: source.clone(),
                }
            });
        };
        let after_flush = preceding
            .get(flush_position.saturating_add(1)..)
            .unwrap_or(&[]);
        if after_flush
            .iter()
            .any(|later| matches!(later, Op::Write { path, .. } if path == source))
        {
            return Err(Violation::WriteAfterFlush {
                path: source.clone(),
            });
        }
    }
    if mutated {
        Ok(())
    } else {
        Err(Violation::NothingPublished)
    }
}

/// The directory whose entry list an operation changed, if it changed one.
fn namespace_mutation_directory(op: &Op) -> Option<&Path> {
    match op {
        Op::Replace { to, .. } | Op::Create { to, .. } | Op::Delete(to) => to.parent(),
        _ => None,
    }
}

/// Access mask used for every artifact this protocol writes.
const WRITE_ACCESS: win32::DWORD = win32::GENERIC_READ | win32::GENERIC_WRITE;

/// Sharing mask used while an artifact is being written: no other opener.
const WRITE_SHARE: win32::DWORD = 0;

/// Flags `MoveFileExW` would need for an in-place replacement.
///
/// **Measured and rejected.** `tests/diagnostics.rs::replacement_matrix_over_an_open_reader`
/// shows `MoveFileExW` returning `ERROR_ACCESS_DENIED` whenever *any* handle to
/// the destination is open, including a reader that opened with
/// `FILE_SHARE_DELETE`. `MOVEFILE_WRITE_THROUGH` changes nothing. The engine
/// must be able to republish a manifest while readers hold it, so publication
/// uses the POSIX-semantics rename below instead and this constant exists only
/// to keep the rejected candidate named and testable.
pub const REJECTED_MOVEFILE_FLAGS: win32::DWORD =
    win32::MOVEFILE_REPLACE_EXISTING | win32::MOVEFILE_WRITE_THROUGH;

/// Makes a directory entry itself durable.
///
/// This is the Windows analogue of `fsync` on a directory descriptor.
/// `FILE_FLAG_BACKUP_SEMANTICS` is required to obtain a directory handle at
/// all, and the handle must carry write access: measured on local NTFS as a
/// standard user, a `GENERIC_READ` or metadata-only directory handle returns
/// `ERROR_ACCESS_DENIED` from `FlushFileBuffers`, while `GENERIC_WRITE`
/// succeeds. No elevation is involved and no volume handle is opened.
pub fn flush_namespace(directory: &Path) -> io::Result<()> {
    let handle = win32::open_directory(directory, win32::GENERIC_WRITE)?;
    win32::flush(&handle)
}

/// Writes `bytes` to a same-directory temporary and publishes it as `final_path`.
///
/// Returns the trace so a caller (or the oracle) can inspect exactly what ran.
/// `variant` selects the correct protocol or one of the deliberate violations.
pub fn publish(
    final_path: &Path,
    bytes: &[u8],
    variant: Variant,
    log: &mut OpLog,
) -> io::Result<()> {
    let directory = final_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "publication target has no parent")
    })?;
    let file_name = final_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "publication target has no name")
    })?;
    let mut temporary_name = file_name.to_os_string();
    temporary_name.push(".tmp-publish");
    let temporary = directory.join(temporary_name);

    let existed = final_path.try_exists()?;

    let handle = win32::create_file(
        &temporary,
        WRITE_ACCESS,
        WRITE_SHARE,
        win32::CREATE_ALWAYS,
        win32::FILE_ATTRIBUTE_NORMAL,
    )?;
    log.push(Op::OpenWrite(temporary.clone()));

    win32::write_all(&handle, bytes)?;
    log.push(Op::Write {
        path: temporary.clone(),
        bytes: bytes.len(),
    });

    let flush_before_publish = matches!(variant, Variant::Correct);
    if flush_before_publish {
        win32::flush(&handle)?;
        log.push(Op::FlushData(temporary.clone()));
    }

    // The handle is closed before the rename. A handle opened with share mode 0
    // would otherwise block the replacement itself.
    drop(handle);
    log.push(Op::Close(temporary.clone()));

    // `std::fs::rename` issues a POSIX-semantics `FileRenameInformationEx`
    // before falling back to `MoveFileExW`. That is the only measured route
    // that can replace a name a reader still holds open.
    std::fs::rename(&temporary, final_path)?;
    log.push(if existed {
        Op::Replace {
            from: temporary.clone(),
            to: final_path.to_path_buf(),
        }
    } else {
        Op::Create {
            from: temporary.clone(),
            to: final_path.to_path_buf(),
        }
    });

    if matches!(variant, Variant::FlushAfterPublish) {
        // Reopen the published name and flush it now: the bytes do reach the
        // media, but only after the name was already visible.
        let reopened = win32::create_file(
            final_path,
            WRITE_ACCESS,
            win32::FILE_SHARE_READ,
            win32::OPEN_EXISTING,
            win32::FILE_ATTRIBUTE_NORMAL,
        )?;
        win32::flush(&reopened)?;
        log.push(Op::FlushData(temporary));
    }

    if !matches!(variant, Variant::MissingNamespaceFlush) {
        flush_namespace(directory)?;
        log.push(Op::FlushNamespace(directory.to_path_buf()));
    }

    Ok(())
}

/// Unlinks a retired artifact and makes its removal durable.
pub fn retire(path: &Path, log: &mut OpLog) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "retirement target has no parent")
    })?;
    win32::delete_file(path)?;
    log.push(Op::Delete(path.to_path_buf()));
    flush_namespace(directory)?;
    log.push(Op::FlushNamespace(directory.to_path_buf()));
    Ok(())
}

/// Appends to a WAL-style file through a retained handle and flushes it.
///
/// Returns the handle so the caller can prove that repeated appends and
/// flushes reach the same file object rather than a reopened path.
pub fn open_append(path: &Path) -> io::Result<win32::Handle> {
    // `OPEN_ALWAYS` creates the log on first use and opens it afterwards.
    win32::create_file(
        path,
        WRITE_ACCESS,
        win32::FILE_SHARE_READ,
        win32::OPEN_ALWAYS,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
}

/// True when both paths resolve to the same volume, which `MOVEFILE_REPLACE_EXISTING`
/// requires: without `MOVEFILE_COPY_ALLOWED` a cross-volume move fails, and with
/// it the operation would stop being an atomic replacement.
pub fn same_volume(left: &Path, right: &Path) -> io::Result<bool> {
    let left_handle = win32::open_directory(left, win32::GENERIC_READ)?;
    let right_handle = win32::open_directory(right, win32::GENERIC_READ)?;
    let (left_volume, _) = win32::file_identity(&left_handle)?;
    let (right_volume, _) = win32::file_identity(&right_handle)?;
    Ok(left_volume == right_volume)
}

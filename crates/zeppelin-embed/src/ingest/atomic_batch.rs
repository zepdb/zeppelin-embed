//! Crash atomicity of one mutation batch (ZE-216).
//!
//! One `Store::ingest` or `Store::delete` call is one batch. After a crash at
//! any byte of its WAL append, reopen shows the whole batch or none of it:
//!
//! - An interrupted final append leaves the last record shorter than its
//!   declared length. That append never returned, so nobody was told it
//!   committed. The writer cuts it off at open ([`cut_interrupted_append`]).
//! - A batch of several records can also stop at a record boundary, when the
//!   crash falls between two WAL groups. Its records carry their position
//!   (`UPSERT_V2_BATCH_MEMBER`), and replay applies a batch only when every
//!   member is present ([`committed_mutations`]).

use std::path::Path;

use crate::lifecycle::StoreError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::vfs::Vfs;
use crate::wal::record::RecordError;
use crate::wal::replay::{CorruptionLocation, CorruptionReason, ReplayTerminator, replay};
use crate::wal::{DEFAULT_MAX_GROUP_BYTES_DURABLE, LogSeq, VisibleRecord};

use super::IngestError;
use super::wal_payload::{self, MutationPayload, PayloadError};

/// Same temporary name the purge rewrite uses, so a crash between write and
/// rename leaves no new kind of orphan file.
const CUT_TEMPORARY: &str = ".wal.ze.purge.tmp";

/// Cuts an interrupted final append off `directory/wal.ze` before the writer
/// resumes, and reports whether it cut anything.
///
/// Only a final record that is shorter than its declared length qualifies,
/// and only with a length a store writer can produce. Every other WAL damage
/// (a checksum mismatch, a sequence break, a bad header) still fails the open
/// loudly. The checksum-valid prefix is rewritten through a temporary and an
/// atomic rename under the store's sync policy, so a crash during the cut
/// leaves either the torn log or the cut log, and both reopen to the same
/// state. Only the single writer calls this; a read-only open never repairs
/// and refuses a torn tail.
///
/// A repair must never persist a bad read. The WAL bytes must match the file
/// length, and the temporary must read back as exactly the prefix before it
/// replaces the log; otherwise nothing is cut and the ordinary open path
/// reports what it finds.
pub(crate) fn cut_interrupted_append(
    vfs: &dyn Vfs,
    directory: &Path,
    policy: DurabilityPolicy,
) -> Result<bool, StoreError> {
    let path = directory.join("wal.ze");
    let length = match vfs.open(&path) {
        Ok(length) => length,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(StoreError::Io { path, source }),
    };
    let bytes = vfs.read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    if u64::try_from(bytes.len()).ok() != Some(length) {
        return Ok(false);
    }
    let offset = match replay(&bytes).terminator {
        ReplayTerminator::CorruptAt {
            offset,
            reason:
                CorruptionReason::Record {
                    location: CorruptionLocation::Tail,
                    error,
                },
        } if interrupted_append(error) => offset,
        _ => return Ok(false),
    };
    let prefix = bytes.get(..offset).ok_or(StoreError::Synchronization {
        component: "interrupted WAL append offset",
    })?;
    let temporary = directory.join(CUT_TEMPORARY);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| StoreError::Io { path, source }
    };
    vfs.write(&temporary, prefix).map_err(io(&temporary))?;
    if let SyncRequirement::Sync(kind) = policy.data_file_sync() {
        vfs.sync(&temporary, kind).map_err(io(&temporary))?;
    }
    if vfs.read(&temporary).map_err(io(&temporary))? != prefix {
        return Ok(false);
    }
    vfs.rename(&temporary, &path).map_err(io(&path))?;
    if let SyncRequirement::Sync(kind) = policy.directory_sync() {
        vfs.sync(directory, kind).map_err(io(directory))?;
    }
    Ok(true)
}

/// A record torn by the end of the file: its header or body stops early, and
/// its declared length fits the largest group a store writer appends.
fn interrupted_append(error: RecordError) -> bool {
    match error {
        RecordError::HeaderTruncated { .. } => true,
        RecordError::BodyTruncated { needed, .. } => needed <= DEFAULT_MAX_GROUP_BYTES_DURABLE,
        RecordError::LengthOverflow { .. } | RecordError::ChecksumMismatch { .. } => false,
    }
}

/// Frames the upsert records of one batch as members `0..count` when the
/// batch writes more than one record; a one-record batch stays standalone.
pub(crate) fn frame_batch(
    records: Vec<(usize, u16, Vec<u8>)>,
) -> Result<Vec<(usize, u16, Vec<u8>)>, IngestError> {
    if records.len() < 2 {
        return Ok(records);
    }
    let count = u32::try_from(records.len())
        .map_err(|_| IngestError::Payload(PayloadError::LengthOverflow))?;
    records
        .into_iter()
        .zip(0_u32..)
        .map(|((row, op, payload), index)| {
            if op != wal_payload::UPSERT_V2 {
                return Err(IngestError::Payload(PayloadError::UnknownOperation(op)));
            }
            wal_payload::encode_upsert_v2_batch_member(index, count, &payload)
                .map(|member| (row, wal_payload::UPSERT_V2_BATCH_MEMBER, member))
                .map_err(IngestError::Payload)
        })
        .collect()
}

/// Decodes the records after `absorbed_through` into the mutations that
/// committed, in log order.
///
/// Standalone records always committed. Batch members commit only as a
/// complete run `0..count`; a run that the log end, a new member 0, or a
/// standalone record cuts short belongs to an append that never returned and
/// is dropped whole. A member that continues nothing is corruption. A seal
/// absorbs through a record only after its batch returned, so `absorbed_through`
/// never falls inside a batch.
pub(crate) fn committed_mutations(
    records: &[VisibleRecord],
    absorbed_through: u64,
) -> Result<Vec<(LogSeq, u16, MutationPayload)>, StoreError> {
    committed_mutations_with_decisions(records, absorbed_through, |_| None)
}

pub(crate) fn committed_mutations_with_decisions(
    records: &[VisibleRecord],
    absorbed_through: u64,
    decision: impl Fn(wal_payload::TransactionBinding) -> Option<wal_payload::TransactionBinding>,
) -> Result<Vec<(LogSeq, u16, MutationPayload)>, StoreError> {
    use wal_payload::{PreparedMutation, TransactionDecision, transaction_decision};
    let mut prepared_run: Vec<(LogSeq, PreparedMutation)> = Vec::new();
    let mut committed = Vec::new();
    let mut run: Vec<(LogSeq, u16, MutationPayload)> = Vec::new();
    let mut run_count = 0_u32;
    for record in records {
        if record.seq.get() <= absorbed_through {
            continue;
        }
        let payload = record.payload().map_err(|source| StoreError::WalRecord {
            seq: record.seq,
            source,
        })?;
        if record.op == wal_payload::PREPARED_MUTATION_V1 {
            // A writer may resume after a crash left whole members of an
            // unreturned local batch at the tail. Preparation cuts it short.
            run.clear();
            let member = wal_payload::decode_prepared(payload).map_err(|source| {
                StoreError::WalMutation {
                    seq: record.seq,
                    op: record.op,
                    source,
                }
            })?;
            let mismatch = || StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source: PayloadError::TransactionBinding,
            };
            if member
                .binding
                .first_seq
                .checked_add(u64::from(member.index))
                != Some(record.seq.get())
            {
                return Err(mismatch());
            }
            if member.index == 0 {
                if prepared_run.first().is_some_and(|(_, prior)| {
                    transaction_decision(prior.binding, decision(prior.binding))
                        == TransactionDecision::Committed
                }) {
                    return Err(mismatch());
                }
                prepared_run.clear();
            }
            if usize::try_from(member.index).ok() != Some(prepared_run.len())
                || prepared_run
                    .first()
                    .is_some_and(|(_, first)| first.binding != member.binding)
            {
                return Err(mismatch());
            }
            let status = transaction_decision(member.binding, decision(member.binding));
            if status == TransactionDecision::Mismatch {
                return Err(mismatch());
            }
            let complete = member.index.checked_add(1) == Some(member.count);
            prepared_run.push((record.seq, member));
            if complete {
                if status == TransactionDecision::Committed {
                    committed.extend(
                        prepared_run
                            .drain(..)
                            .map(|(seq, member)| (seq, member.op, member.mutation)),
                    );
                } else {
                    prepared_run.clear();
                }
            }
            continue;
        }
        if let Some((_, member)) = prepared_run.first() {
            if transaction_decision(member.binding, decision(member.binding))
                == TransactionDecision::Committed
            {
                return Err(StoreError::WalMutation {
                    seq: record.seq,
                    op: record.op,
                    source: PayloadError::TransactionBinding,
                });
            }
        }
        prepared_run.clear();
        let mutation = wal_payload::decode_mutation(record.op, payload).map_err(|source| {
            StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source,
            }
        })?;
        let MutationPayload::BatchMember {
            index,
            count,
            document,
        } = mutation
        else {
            run.clear();
            committed.push((record.seq, record.op, mutation));
            continue;
        };
        if index == 0 {
            run.clear();
            run_count = count;
        } else if count != run_count || usize::try_from(index).ok() != Some(run.len()) {
            return Err(StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source: PayloadError::OrphanBatchMember { index, count },
            });
        }
        run.push((record.seq, record.op, MutationPayload::Upsert(document)));
        if index.checked_add(1) == Some(count) {
            committed.append(&mut run);
        }
    }
    if let Some((seq, member)) = prepared_run.first() {
        if transaction_decision(member.binding, decision(member.binding))
            == TransactionDecision::Committed
        {
            return Err(StoreError::WalMutation {
                seq: *seq,
                op: wal_payload::PREPARED_MUTATION_V1,
                source: PayloadError::TransactionBinding,
            });
        }
    }
    Ok(committed)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::cut_interrupted_append;
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::vfs::crash::MemoryVfs;
    use crate::vfs::{SyncKind, Vfs, VfsFile};
    use crate::wal::{LogSeq, encode_wal_image};

    /// Delegates to memory, but returns a halved buffer for read number
    /// `short_read` (1-based), the way a content fault or short read would.
    struct ShortReadVfs {
        inner: MemoryVfs,
        short_read: usize,
        reads: AtomicUsize,
    }

    impl Vfs for ShortReadVfs {
        fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
            self.inner.ensure_directory(path, create)
        }
        fn open(&self, path: &Path) -> std::io::Result<u64> {
            self.inner.open(path)
        }
        fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
            self.inner.open_for_map(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            let mut bytes = self.inner.read(path)?;
            if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.short_read {
                bytes.truncate(bytes.len() / 2);
            }
            Ok(bytes)
        }
        fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
            self.inner.read_range(path, offset, length)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.write(path, bytes)
        }
        fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
            self.inner.open_append(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.inner.rename(from, to)
        }
        fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
            self.inner.sync(path, kind)
        }
        fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
            self.inner.list(directory)
        }
        fn for_each_direct_child(
            &self,
            directory: &Path,
            visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
        ) -> std::io::Result<()> {
            self.inner.for_each_direct_child(directory, visitor)
        }
        fn delete(&self, path: &Path) -> std::io::Result<()> {
            self.inner.delete(path)
        }
    }

    /// Runs one cut over a two-record WAL whose second record lost its last
    /// byte. Returns (cut, WAL after, torn WAL, clean one-record prefix).
    fn cut_with_short_read(short_read: usize) -> (bool, Vec<u8>, Vec<u8>, Vec<u8>) {
        let full = encode_wal_image(LogSeq::new(1), &[(7, vec![1; 40]), (7, vec![2; 40])])
            .expect("encode WAL");
        let prefix = encode_wal_image(LogSeq::new(1), &[(7, vec![1; 40])]).expect("prefix");
        let torn = full.get(..full.len() - 1).expect("torn WAL").to_vec();
        let vfs = ShortReadVfs {
            inner: MemoryVfs::new(),
            short_read,
            reads: AtomicUsize::new(0),
        };
        let directory = Path::new("/store");
        vfs.inner
            .insert(directory.join("wal.ze"), torn.clone())
            .expect("seed WAL");
        let policy =
            DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable).expect("policy");
        let cut = cut_interrupted_append(&vfs, directory, policy).expect("cut");
        let after = vfs.inner.read(&directory.join("wal.ze")).expect("read WAL");
        (cut, after, torn, prefix)
    }

    #[test]
    fn an_interrupted_final_append_is_cut_to_its_clean_prefix() {
        let (cut, after, _, prefix) = cut_with_short_read(0);
        assert!(cut);
        assert_eq!(after, prefix);
    }

    #[test]
    fn a_short_wal_read_is_never_persisted() {
        let (cut, after, torn, _) = cut_with_short_read(1);
        assert!(!cut);
        assert_eq!(after, torn);
    }

    #[test]
    fn a_temporary_that_reads_back_wrong_never_replaces_the_wal() {
        let (cut, after, torn, _) = cut_with_short_read(2);
        assert!(!cut);
        assert_eq!(after, torn);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod prepared_tests {
    use super::*;
    use crate::vfs::crash::MemoryVfs;
    use crate::wal::{WalReader, encode_wal_image};
    fn binding() -> wal_payload::TransactionBinding {
        wal_payload::TransactionBinding {
            transaction: 1,
            participant: 2,
            first_seq: 3,
            last_seq: 4,
            manifest_digest: 5,
            final_generation: 6,
        }
    }
    fn records(members: &[(wal_payload::TransactionBinding, u32)]) -> WalReader {
        let body = wal_payload::encode_delete(&[crate::ingest::DocId::new(7)]).expect("delete");
        let frames = members
            .iter()
            .map(|(binding, index)| {
                (
                    wal_payload::PREPARED_MUTATION_V1,
                    wal_payload::encode_prepared(
                        *binding,
                        *index,
                        2,
                        wal_payload::DELETE_V1,
                        &body,
                    )
                    .expect("member"),
                )
            })
            .collect::<Vec<_>>();
        let image = encode_wal_image(LogSeq::new(3), &frames).expect("WAL");
        let vfs = MemoryVfs::new();
        vfs.insert(Path::new("wal"), image).expect("seed");
        WalReader::open(&vfs, Path::new("wal")).expect("reader")
    }
    #[test]
    fn prepared_frames_require_exact_complete_decision() {
        let reader = records(&[(binding(), 0), (binding(), 1)]);
        assert!(
            committed_mutations(reader.records(), 0)
                .expect("undecided")
                .is_empty()
        );
        assert_eq!(
            committed_mutations_with_decisions(reader.records(), 0, |_| Some(binding()))
                .expect("committed")
                .len(),
            2
        );
        let mut wrong = binding();
        wrong.manifest_digest += 1;
        assert!(committed_mutations_with_decisions(reader.records(), 0, |_| Some(wrong)).is_err());
        let partial = records(&[(binding(), 0)]);
        assert!(
            committed_mutations(partial.records(), 0)
                .expect("undecided prefix")
                .is_empty()
        );
        assert!(
            committed_mutations_with_decisions(partial.records(), 0, |_| Some(binding())).is_err()
        );
    }
    #[test]
    fn a_new_preparation_cannot_hide_an_incomplete_committed_range() {
        let mut later = binding();
        later.transaction = 9;
        later.first_seq = 4;
        later.last_seq = 5;
        let reader = records(&[(binding(), 0), (later, 0)]);
        assert!(
            committed_mutations_with_decisions(reader.records(), 0, |evidence| {
                if evidence.transaction == 1 {
                    Some(binding())
                } else {
                    None
                }
            })
            .is_err()
        );
    }
}

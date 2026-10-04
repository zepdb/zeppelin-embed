//! ZE-239: private prepared stores, selected by one durable root record.
//!
//! The root record is the only publication point. Old stores and abandoned
//! preparations are retained, so readers never depend on sibling recovery.
use super::durability::{CommitTier, DurabilityMode};
use super::lock::StoreLock;
use super::{AccessMode, OpenOptions, Store, StoreError};
use crate::ingest::{DeleteBatch, DocId, IngestBatch, IngestDocument};
use crate::meta::Predicate;
use crate::vfs::{StdVfs, SyncKind, Vfs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use xxhash_rust::xxh3::xxh3_64;

const RECORD: &str = ".ze-namespaces";
const REFERENCE: &str = ".ze-namespace-root";
const PREPARED: &str = ".ze-prepared";
const MAGIC: &[u8] = b"ZENS0001";
const MAX_RECORD: usize = 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);
type Routes = BTreeMap<String, String>;

/// One namespace's ordered upserts, deletes, then predicate delete.
pub struct NamespaceMutation {
    /// Existing direct child namespace name, using the namespace-open grammar.
    pub name: String,
    /// Existing namespace's schema, epoch and tokenizer declaration.
    pub options: OpenOptions,
    /// Documents to upsert.
    pub upserts: Vec<IngestDocument>,
    /// IDs to delete.
    pub deletes: Vec<DocId>,
    /// Predicate evaluated after the explicit changes, in the private state.
    pub delete_where: Option<Predicate>,
}

pub(super) fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}
pub(super) fn invalid(path: &Path, message: &str) -> StoreError {
    io(
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    )
}
pub(super) fn name_valid(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
pub(super) fn envelope(body: &[u8]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    bytes
}
pub(super) fn body<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a [u8], StoreError> {
    let end = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| invalid(path, "short namespace record"))?;
    let prefix = bytes
        .get(..end)
        .ok_or_else(|| invalid(path, "namespace record bounds"))?;
    if bytes.len() > MAX_RECORD
        || !prefix.starts_with(MAGIC)
        || bytes.get(end..) != Some(xxh3_64(prefix).to_le_bytes().as_slice())
    {
        return Err(invalid(path, "namespace record checksum/version"));
    }
    prefix
        .get(MAGIC.len()..)
        .ok_or_else(|| invalid(path, "namespace record header"))
}
pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match StdVfs.open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
        Ok(n) if n > MAX_RECORD as u64 => Err(invalid(path, "namespace record too large")),
        Ok(_) => StdVfs.read(path).map(Some).map_err(|e| io(path, e)),
    }
}
fn encode(routes: &Routes) -> Vec<u8> {
    // Names and destinations are restricted ASCII without separators except
    // the one slash separating a preparation directory from its namespace.
    let mut text = String::new();
    for (name, destination) in routes {
        text.push_str(name);
        text.push('\t');
        text.push_str(destination);
        text.push('\n');
    }
    envelope(text.as_bytes())
}
enum RootDescriptor {
    Legacy(Routes),
    Staged(StagedDescriptor),
}
fn routes(root: &Path) -> Result<Routes, StoreError> {
    match root_descriptor(root)? {
        RootDescriptor::Legacy(routes) => Ok(routes),
        RootDescriptor::Staged(_) => Err(invalid(
            &root.join(RECORD),
            "staged manifest requires transaction adoption",
        )),
    }
}
fn root_descriptor(root: &Path) -> Result<RootDescriptor, StoreError> {
    let path = root.join(RECORD);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(RootDescriptor::Legacy(Routes::new()));
    };
    if bytes.starts_with(STAGED_MAGIC) {
        return decode_staged(&path, &bytes).map(RootDescriptor::Staged);
    }
    let text = std::str::from_utf8(body(&path, &bytes)?)
        .map_err(|_| invalid(&path, "namespace record UTF-8"))?;
    let mut result = Routes::new();
    for line in text.split_terminator('\n') {
        let (name, destination) = line
            .split_once('\t')
            .ok_or_else(|| invalid(&path, "namespace route"))?;
        let (transaction, child) = destination
            .split_once('/')
            .ok_or_else(|| invalid(&path, "namespace destination"))?;
        if !name_valid(name)
            || child != name
            || !transaction.starts_with(".ze-batch-")
            || !transaction
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            || transaction.contains("..")
            || result
                .insert(name.to_owned(), destination.to_owned())
                .is_some()
        {
            return Err(invalid(&path, "invalid or duplicate namespace route"));
        }
    }
    if encode(&result) != bytes {
        return Err(invalid(&path, "noncanonical namespace record"));
    }
    Ok(RootDescriptor::Legacy(result))
}

/// Selects one complete committed namespace. No sibling is opened or repaired.
pub(super) fn resolve(path: &Path) -> Result<PathBuf, StoreError> {
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(StoreError::NotDirectory {
                path: path.to_path_buf(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(path, error)),
    }
    let Some(parent) = path.parent() else {
        return Ok(path.to_path_buf());
    };
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Ok(path.to_path_buf());
    };
    let reference = read_optional(&path.join(REFERENCE))?;
    if let Some(reference) = reference {
        let expected = body(&path.join(REFERENCE), &reference)?;
        let canonical = std::fs::canonicalize(parent).map_err(|e| io(parent, e))?;
        if canonical.to_str().map(str::as_bytes) != Some(expected)
            || read_optional(&parent.join(RECORD))?.is_none()
        {
            return Err(invalid(
                path,
                "namespace requires its original transaction root",
            ));
        }
    }
    let selected = match root_descriptor(parent)? {
        RootDescriptor::Legacy(mut routes) => routes.remove(name),
        RootDescriptor::Staged(descriptor) => {
            if descriptor.0.contains_key(name) {
                return Err(invalid(
                    path,
                    "selected staged manifest requires transaction adoption",
                ));
            }
            return Ok(path.to_path_buf());
        }
    };
    match selected {
        None => Ok(path.to_path_buf()),
        Some(destination) => {
            let selected = parent.join(&destination);
            let transaction = selected
                .parent()
                .ok_or_else(|| invalid(path, "missing transaction directory"))?;
            let intent_path = transaction.join("intent.ze");
            let intent = read_optional(&intent_path)?
                .ok_or_else(|| invalid(&intent_path, "missing transaction intent"))?;
            let intent_body = std::str::from_utf8(body(&intent_path, &intent)?)
                .map_err(|_| invalid(&intent_path, "intent UTF-8"))?;
            if !intent_body
                .lines()
                .any(|line| line == format!("{name}\t{destination}"))
            {
                return Err(invalid(
                    &intent_path,
                    "decision disagrees with preparation intent",
                ));
            }
            let prepared_path = selected.join(PREPARED);
            let bytes = read_optional(&prepared_path)?
                .ok_or_else(|| invalid(&prepared_path, "missing namespace preparation"))?;
            if body(&prepared_path, &bytes)? != destination.as_bytes() {
                return Err(invalid(
                    &prepared_path,
                    "namespace preparation identity mismatch",
                ));
            }
            // Ordinary store open may create or accept an empty store. A
            // committed preparation is never such a store: these files were
            // required and synced before its decision, so loss is corruption.
            for filename in ["manifest.ze", "wal.ze"] {
                let required = selected.join(filename);
                let length = StdVfs.open(&required).map_err(|e| io(&required, e))?;
                if length == 0 {
                    return Err(invalid(&required, "empty committed preparation file"));
                }
            }
            Ok(selected)
        }
    }
}

pub(super) fn durable_write(
    vfs: &dyn Vfs,
    path: &Path,
    bytes: &[u8],
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    vfs.write(path, bytes).map_err(|e| io(path, e))?;
    step("write").map_err(|e| io(path, e))?;
    vfs.sync(path, SyncKind::Full).map_err(|e| io(path, e))?;
    step("file sync").map_err(|e| io(path, e))
}
pub(super) fn sync_dir(
    vfs: &dyn Vfs,
    path: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    vfs.sync(path, SyncKind::Full).map_err(|e| io(path, e))?;
    step("directory sync").map_err(|e| io(path, e))
}
fn publish(
    vfs: &dyn Vfs,
    root: &Path,
    bytes: &[u8],
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    let temporary = root.join(".ze-namespaces.tmp");
    durable_write(vfs, &temporary, bytes, step)?;
    vfs.rename(&temporary, &root.join(RECORD))
        .map_err(|e| io(root, e))?;
    step("commit rename").map_err(|e| io(root, e))?;
    sync_dir(vfs, root, step)
}

// The coordinator exclusively owns this source Store and never exposes its
// handle. Copy its validated disk image directly: purge may have rewritten the
// WAL sequence origin, which a concurrent snapshot's retained-tail seam rejects.
fn prepare_copy(
    source: &Store,
    destination: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    StdVfs
        .create_directory(destination)
        .map_err(|e| io(destination, e))?;
    step("participant directory").map_err(|e| io(destination, e))?;
    for path in source
        .vfs
        .list(&source.directory)
        .map_err(|e| io(&source.directory, e))?
    {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(invalid(&path, "store filename is not UTF-8"));
        };
        if name != "manifest.ze" && name != "wal.ze" && !name.ends_with(".zseg") {
            continue;
        }
        let length = source.vfs.open(&path).map_err(|e| io(&path, e))?;
        let target = destination.join(name);
        let mut file = StdVfs.open_append(&target).map_err(|e| io(&target, e))?;
        let mut offset = 0;
        while offset < length {
            let count = usize::try_from((length - offset).min(1024 * 1024))
                .map_err(|_| invalid(&path, "copy size"))?;
            let bytes = source
                .vfs
                .read_range(&path, offset, count)
                .map_err(|e| io(&path, e))?;
            if bytes.len() != count {
                return Err(invalid(&path, "short preparation read"));
            }
            file.append(&bytes).map_err(|e| io(&target, e))?;
            offset += bytes.len() as u64;
            step("copy chunk").map_err(|e| io(&target, e))?;
        }
        file.sync(SyncKind::Full).map_err(|e| io(&target, e))?;
        step("copied file sync").map_err(|e| io(&target, e))?;
    }
    sync_dir(&StdVfs, destination, step)
}

/// Atomically selects privately prepared namespace stores with one root commit.
///
/// All participants must exist and have no open writable handle or in-place
/// snapshot view (even after its source writer closes). Existing
/// read-only handles retain their old snapshots. New direct/namespace opens
/// select the complete root decision without recovering other namespaces.
/// This call always fully syncs its data. An I/O error near commit is an
/// indeterminate outcome: reopen to discover the decision before retrying.
/// Preparations and previous stores are retained (including deleted bytes).
/// Only logical deletion is promised here; physical reclamation is deferred.
/// Returned generations follow input order. Ordinary writes do no root I/O.
pub fn namespace_batch(
    root: &Path,
    mutations: Vec<NamespaceMutation>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, mutations, Operation::Batch, &mut |_| Ok(()))
}

/// Protocol interruption seam for kill/fault tests; never enabled by environment.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn namespace_batch_with_steps(
    root: &Path,
    mutations: Vec<NamespaceMutation>,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, mutations, Operation::Batch, step)
}

/// Durably declares a cascade while owning the root and participant writer locks.
/// Participants supply the existing parent/child specs; mutation fields must be empty.
pub fn namespace_declare_cascade(
    root: &Path,
    participants: Vec<NamespaceMutation>,
    rule: super::CascadeRule,
) -> Result<(), StoreError> {
    execute(
        root,
        participants,
        Operation::Declare(rule),
        &mut |_| Ok(()),
    )
    .map(|_| ())
}

/// Deletes explicit IDs and all declared transitive dependants in one root commit.
/// Supply every reachable namespace with its existing options. Only `deletes`
/// may be populated. All ZE-239 closed-writer and logical-deletion limits apply.
pub fn namespace_delete_cascade(
    root: &Path,
    participants: Vec<NamespaceMutation>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, participants, Operation::Cascade, &mut |_| Ok(()))
}

/// Protocol interruption seam for cascade kill tests.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn namespace_delete_cascade_with_steps(
    root: &Path,
    participants: Vec<NamespaceMutation>,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, participants, Operation::Cascade, step)
}

enum Operation {
    Batch,
    Cascade,
    Declare(super::CascadeRule),
}

fn execute(
    root: &Path,
    mut mutations: Vec<NamespaceMutation>,
    operation: Operation,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    let root = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
    let minimum = if matches!(operation, Operation::Batch) {
        2
    } else {
        1
    };
    if mutations.len() < minimum || mutations.len() > 128 {
        return Err(invalid(
            &root,
            "namespace batch requires 2..128 participants",
        ));
    }
    if !matches!(operation, Operation::Batch)
        && mutations.iter().any(|m| {
            !m.upserts.is_empty()
                || m.delete_where.is_some()
                || (matches!(operation, Operation::Declare(_)) && !m.deletes.is_empty())
        })
    {
        return Err(invalid(
            &root,
            "cascade operations reject upserts/filters; declarations reject deletes",
        ));
    }
    let _coordinator = StoreLock::acquire(&root).map_err(StoreError::Lock)?;
    let mut next = routes(&root)?;
    let mut ordered = BTreeMap::new();
    for (index, mutation) in mutations.iter().enumerate() {
        if !name_valid(&mutation.name)
            || ordered.insert(mutation.name.clone(), index).is_some()
            || mutation.options.access_mode != AccessMode::ReadWrite
        {
            return Err(invalid(
                &root,
                "invalid, duplicate, or read-only namespace participant",
            ));
        }
    }
    // Opening originals also owns their stable logical writer locks. Never
    // mutate these stores: all validation and mutation happens on private copies.
    let mut sources = BTreeMap::new();
    for (name, index) in &ordered {
        let mutation = mutations
            .get(*index)
            .ok_or_else(|| invalid(&root, "participant index"))?;
        let path = root.join(name);
        if std::fs::symlink_metadata(&path)
            .map_err(|e| io(&path, e))?
            .file_type()
            .is_symlink()
        {
            return Err(invalid(
                &path,
                "namespace links are not transaction participants",
            ));
        }
        let mut options = mutation.options.clone();
        options.access_mode = AccessMode::ReadOnly;
        // Validate declarations without allowing additive schema evolution.
        drop(Store::open(&path, options)?);
        sources.insert(name.clone(), Store::open(&path, mutation.options.clone())?);
    }
    match operation {
        Operation::Declare(rule) => {
            super::cascade::declare(&root, &sources, rule, step)?;
            return Ok(Vec::new());
        }
        Operation::Cascade => super::cascade::expand(&root, &sources, &mut mutations)?,
        Operation::Batch => {}
    }
    let transaction = loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(".ze-batch-{}-{id}", std::process::id()));
        match StdVfs.create_directory(&path) {
            Ok(()) => break path,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(&path, e)),
        }
    };
    step("transaction directory").map_err(|e| io(&transaction, e))?;
    let transaction_name = transaction
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid(&transaction, "transaction name"))?;
    for name in ordered.keys() {
        next.insert(name.clone(), format!("{transaction_name}/{name}"));
    }
    let decision = encode(&next);
    if decision.len() > MAX_RECORD {
        return Err(invalid(&root, "namespace decision too large"));
    }
    durable_write(&StdVfs, &transaction.join("intent.ze"), &decision, step)?;
    sync_dir(&StdVfs, &transaction, step)?;
    sync_dir(&StdVfs, &root, step)?;
    let mut generations = Vec::new();
    for mutation in mutations {
        let source = sources
            .get(&mutation.name)
            .ok_or_else(|| invalid(&root, "missing participant"))?;
        let destination = transaction.join(&mutation.name);
        prepare_copy(source, &destination, step)?;
        step("snapshot prepared").map_err(|e| io(&destination, e))?;
        let mut options = mutation.options;
        // Even an empty, previously unstamped source gets a manifest in its
        // private copy, making missing prepared files an unambiguous error.
        options.schema = Some(source.schema().clone());
        options.durability_mode = DurabilityMode::Durable;
        options.commit_tier = CommitTier::Durable;
        let store = Store::open(&destination, options)?;
        if !mutation.upserts.is_empty() {
            let mut batch = IngestBatch::new(mutation.upserts);
            if let Some(epoch) = store.epoch_identity() {
                batch = batch.with_epoch(epoch);
            }
            store
                .ingest(batch)
                .map_err(|e| invalid(&destination, &e.to_string()))?;
            step("upserts prepared").map_err(|e| io(&destination, e))?;
        }
        if !mutation.deletes.is_empty() {
            store
                .delete(DeleteBatch::new(mutation.deletes))
                .map_err(|e| invalid(&destination, &e.to_string()))?;
            step("deletes prepared").map_err(|e| io(&destination, e))?;
        }
        if let Some(predicate) = mutation.delete_where {
            store
                .delete_matching(&predicate)
                .map_err(|e| invalid(&destination, &e.to_string()))?;
            step("predicate prepared").map_err(|e| io(&destination, e))?;
        }
        let generation = store.seal()?;
        step("prepared checkpoint").map_err(|e| io(&destination, e))?;
        store.close()?;
        generations.push(generation);
        let relative = next
            .get(&mutation.name)
            .ok_or_else(|| invalid(&root, "missing route"))?;
        durable_write(
            &StdVfs,
            &destination.join(PREPARED),
            &envelope(relative.as_bytes()),
            step,
        )?;
        sync_dir(&StdVfs, &destination, step)?;
    }
    sync_dir(&StdVfs, &transaction, step)?;
    // Create the empty baseline before references, so a crash while enlisting
    // namespaces still resolves to all original stores.
    if read_optional(&root.join(RECORD))?.is_none() {
        publish(&StdVfs, &root, &encode(&Routes::new()), step)?;
    }
    for name in ordered.keys() {
        let directory = root.join(name);
        let reference = directory.join(REFERENCE);
        if read_optional(&reference)?.is_none() {
            let root_text = root
                .to_str()
                .ok_or_else(|| invalid(&root, "root must be UTF-8"))?;
            let temporary = directory.join(".ze-namespace-root.tmp");
            durable_write(&StdVfs, &temporary, &envelope(root_text.as_bytes()), step)?;
            StdVfs
                .rename(&temporary, &reference)
                .map_err(|e| io(&reference, e))?;
            step("reference rename").map_err(|e| io(&reference, e))?;
            sync_dir(&StdVfs, &directory, step)?;
        }
    }
    publish(&StdVfs, &root, &decision, step)?;
    Ok(generations)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vfs::crash::{CrashVfs, MemoryVfs};

    #[test]
    fn root_decision_power_cuts_never_publish_a_torn_or_partial_selection() {
        let root = Path::new("/root");
        let before = encode(&Routes::new());
        let after = encode(&Routes::from([
            ("a".into(), ".ze-batch-1/a".into()),
            ("b".into(), ".ze-batch-1/b".into()),
        ]));
        let memory = MemoryVfs::new();
        memory
            .insert(root.join(RECORD), before.clone())
            .expect("seed decision");
        let crash = CrashVfs::new(memory).expect("recorder");
        publish(&crash, root, &after, &mut |_| Ok(())).expect("publish");
        let states = crash.crash_states().expect("power cuts");
        assert!(!states.was_capped());
        for state in states.iter() {
            let bytes = state
                .vfs()
                .read(&root.join(RECORD))
                .expect("previous or new root");
            assert!(bytes == before || bytes == after, "{:?}", state.kind());
        }
        eprintln!("ZE-239 root power-cut states: {}", states.len());
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod descriptor_tests {
    use super::*;
    #[test]
    fn staged_descriptor_round_trips_and_old_reader_rejects() {
        let binding = crate::ingest::wal_payload::TransactionBinding {
            transaction: 1,
            participant: 2,
            first_seq: 3,
            last_seq: 3,
            manifest_digest: 4,
            final_generation: 5,
        };
        let descriptor = StagedDescriptor(BTreeMap::from([(
            "a".into(),
            StagedSelection {
                manifest: ".ze-manifest-1".into(),
                binding,
            },
        )]));
        let bytes = encode_staged(&descriptor).expect("encode");
        assert_eq!(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "5a454e5330303032010000000100610e002e7a652d6d616e69666573742d3101000000000000000000000000000000020000000000000000000000000000000300000000000000030000000000000004000000000000000500000000000000530352a537e8f10e"
        );
        for end in 0..bytes.len() {
            assert!(decode_staged(Path::new("root"), &bytes[..end]).is_err());
        }
        let mut bad = bytes.clone();
        bad[7] = b'3';
        assert!(decode_staged(Path::new("root"), &bad).is_err());
        let mut bad = bytes.clone();
        bad[40] ^= 1;
        assert!(decode_staged(Path::new("root"), &bad).is_err());
        assert!(body(Path::new("root"), &bytes).is_err());
        assert_eq!(
            decode_staged(Path::new("root"), &bytes).expect("decode"),
            descriptor
        );
    }
}

// ZE-256 v2 is a format seam only. Adoption is deliberately a later step.
// V1 readers reject the distinct magic before interpreting any route.
const STAGED_MAGIC: &[u8] = b"ZENS0002";
#[derive(Clone, Debug, Eq, PartialEq)]
struct StagedSelection {
    manifest: String,
    binding: crate::ingest::wal_payload::TransactionBinding,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct StagedDescriptor(BTreeMap<String, StagedSelection>);

fn encode_staged(descriptor: &StagedDescriptor) -> Result<Vec<u8>, StoreError> {
    let path = Path::new(RECORD);
    let mut bytes = STAGED_MAGIC.to_vec();
    let count =
        u32::try_from(descriptor.0.len()).map_err(|_| invalid(path, "participant count"))?;
    if count == 0 {
        return Err(invalid(path, "empty staged decision"));
    }
    bytes.extend_from_slice(&count.to_le_bytes());
    let mut transaction = None;
    let mut identities = std::collections::BTreeSet::new();
    for (name, selection) in &descriptor.0 {
        if !name_valid(name)
            || !selection.manifest.starts_with(".ze-manifest-")
            || !name_valid(selection.manifest.trim_start_matches('.'))
            || selection.manifest.contains("..")
            || transaction.is_some_and(|id| id != selection.binding.transaction)
            || !identities.insert(selection.binding.participant)
        {
            return Err(invalid(path, "staged participant identity or manifest"));
        }
        transaction = Some(selection.binding.transaction);
        for text in [name, &selection.manifest] {
            let length =
                u16::try_from(text.len()).map_err(|_| invalid(path, "selection length"))?;
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes.extend_from_slice(
            &selection
                .binding
                .encode()
                .map_err(|_| invalid(path, "transaction binding"))?,
        );
    }
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    if bytes.len() > MAX_RECORD {
        return Err(invalid(path, "staged decision too large"));
    }
    Ok(bytes)
}
fn decode_staged(path: &Path, bytes: &[u8]) -> Result<StagedDescriptor, StoreError> {
    let end = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| invalid(path, "short staged decision"))?;
    let prefix = bytes
        .get(..end)
        .ok_or_else(|| invalid(path, "staged bounds"))?;
    if bytes.len() > MAX_RECORD
        || !prefix.starts_with(STAGED_MAGIC)
        || bytes.get(end..) != Some(xxh3_64(prefix).to_le_bytes().as_slice())
    {
        return Err(invalid(path, "staged checksum/version"));
    }
    let mut remaining = prefix
        .get(8..)
        .ok_or_else(|| invalid(path, "staged header"))?;
    fn take<'a>(path: &Path, bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], StoreError> {
        let result = bytes
            .get(..n)
            .ok_or_else(|| invalid(path, "truncated staged decision"))?;
        *bytes = bytes
            .get(n..)
            .ok_or_else(|| invalid(path, "staged bounds"))?;
        Ok(result)
    }
    let count = u32::from_le_bytes(
        take(path, &mut remaining, 4)?
            .try_into()
            .map_err(|_| invalid(path, "count"))?,
    );
    let mut result = BTreeMap::new();
    for _ in 0..count {
        let mut texts = Vec::new();
        for _ in 0..2 {
            let n = u16::from_le_bytes(
                take(path, &mut remaining, 2)?
                    .try_into()
                    .map_err(|_| invalid(path, "length"))?,
            );
            texts.push(
                std::str::from_utf8(take(path, &mut remaining, usize::from(n))?)
                    .map_err(|_| invalid(path, "selection UTF-8"))?
                    .to_owned(),
            );
        }
        let mut texts = texts.into_iter();
        let name = texts.next().ok_or_else(|| invalid(path, "name"))?;
        let manifest = texts.next().ok_or_else(|| invalid(path, "manifest"))?;
        let binding =
            crate::ingest::wal_payload::TransactionBinding::decode(take(path, &mut remaining, 64)?)
                .map_err(|_| invalid(path, "transaction binding"))?;
        if result
            .insert(name, StagedSelection { manifest, binding })
            .is_some()
        {
            return Err(invalid(path, "duplicate participant"));
        }
    }
    let descriptor = StagedDescriptor(result);
    if !remaining.is_empty() || encode_staged(&descriptor)? != bytes {
        return Err(invalid(path, "noncanonical staged decision"));
    }
    Ok(descriptor)
}

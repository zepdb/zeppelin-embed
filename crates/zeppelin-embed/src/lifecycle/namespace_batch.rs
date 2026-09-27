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

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}
fn invalid(path: &Path, message: &str) -> StoreError {
    io(
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    )
}
fn name_valid(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn envelope(body: &[u8]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    bytes
}
fn body<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a [u8], StoreError> {
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
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
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
fn routes(root: &Path) -> Result<Routes, StoreError> {
    let path = root.join(RECORD);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(Routes::new());
    };
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
    Ok(result)
}

/// Selects one complete committed namespace. No sibling is opened or repaired.
pub(super) fn resolve(path: &Path) -> Result<PathBuf, StoreError> {
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
    let selected = routes(parent)?.remove(name);
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

fn durable_write(
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
fn sync_dir(
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
    execute(root, mutations, &mut |_| Ok(()))
}

/// Protocol interruption seam for kill/fault tests; never enabled by environment.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn namespace_batch_with_steps(
    root: &Path,
    mutations: Vec<NamespaceMutation>,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, mutations, step)
}

fn execute(
    root: &Path,
    mutations: Vec<NamespaceMutation>,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    let root = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
    if mutations.len() < 2 || mutations.len() > 128 {
        return Err(invalid(
            &root,
            "namespace batch requires 2..128 participants",
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

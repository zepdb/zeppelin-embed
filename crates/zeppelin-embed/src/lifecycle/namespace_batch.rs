//! Namespace publication through one durable root decision.
//!
//! ZE-256 batches stage changes in stable participant directories. Local
//! acceptance keeps prepared WAL frames committed after root retirement.
//! ZE-239 copy routes remain readable; cascade semantics are unchanged.
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
mod portable;
mod relocate;
pub use portable::NamespaceRootId;
pub use relocate::namespace_relocate;
#[cfg(any(test, feature = "test-seams"))]
pub use relocate::namespace_relocate_with_steps;

// Constructed only by the coordinator for its own unpublished participant.
pub(crate) struct PrivatePreparation {
    root_id: NamespaceRootId,
    name: String,
    directory: PathBuf,
}

impl PrivatePreparation {
    fn new(
        root_id: NamespaceRootId,
        name: &str,
        directory: &Path,
        _coordinator: &StoreLock,
    ) -> Self {
        Self {
            root_id,
            name: name.to_owned(),
            directory: directory.to_path_buf(),
        }
    }
}

pub(super) fn validate_private_preparation(
    vfs: &dyn Vfs,
    directory: &Path,
    authority: Option<&PrivatePreparation>,
) -> Result<(), StoreError> {
    if portable::authority_with_preparation(vfs, directory, authority)?.is_none() {
        return Err(invalid(
            directory,
            "private preparation requires portable authority",
        ));
    }
    Ok(())
}

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

/// A same-process writable participant and its ordered mutations.
pub struct LiveNamespaceMutation<'a> {
    /// Live writable handle for the named namespace.
    pub store: &'a Store,
    /// Namespace declaration and ordered changes.
    pub mutation: NamespaceMutation,
}

/// Commits changes through caller-owned writable namespace handles.
pub fn namespace_batch_live(
    root: &Path,
    participants: Vec<LiveNamespaceMutation<'_>>,
) -> Result<Vec<u64>, StoreError> {
    execute_live(root, participants, &StdVfs, &mut |_| Ok(()), None)
}

/// Explicit protocol interruption seam for deterministic tests.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn namespace_batch_live_with_steps(
    root: &Path,
    participants: Vec<LiveNamespaceMutation<'_>>,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<Vec<u64>, StoreError> {
    execute_live(root, participants, &StdVfs, step, None)
}

/// Protocol VFS seam. Participant handles must use the supplied VFS too.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn namespace_batch_live_on_vfs(
    root: &Path,
    participants: Vec<LiveNamespaceMutation<'_>>,
    vfs: &dyn Vfs,
) -> Result<Vec<u64>, StoreError> {
    execute_live(root, participants, vfs, &mut |_| Ok(()), None)
}

fn execute_live(
    root: &Path,
    mut participants: Vec<LiveNamespaceMutation<'_>>,
    vfs: &dyn Vfs,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
    coordinator: Option<StoreLock>,
) -> Result<Vec<u64>, StoreError> {
    use crate::ingest::wal_payload::{self, TransactionBinding};
    use std::sync::Arc;
    let root = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
    if !(2..=128).contains(&participants.len()) {
        return Err(invalid(
            &root,
            "namespace batch requires 2..128 participants",
        ));
    }
    let _coordinator = match coordinator {
        Some(lock) => lock,
        None => StoreLock::acquire(&root).map_err(StoreError::Lock)?,
    };
    reclamation::run(vfs, &root, step)?;
    let deleting = participants
        .iter()
        .any(|p| !p.mutation.deletes.is_empty() || p.mutation.delete_where.is_some());
    if deleting {
        reclamation::require_retired_erased(vfs, &root)?;
    }
    normalize_accepted(vfs, &root, step)?;
    let (mut root_id, existing_descriptor) = root_record_vfs(vfs, &root)?;
    if read_optional_vfs(vfs, &root.join(RECORD))?.is_none() {
        root_id = Some(NamespaceRootId::generate().map_err(|e| io(&root, e))?);
    }
    let existing_routes = match existing_descriptor {
        RootDescriptor::Legacy(routes) => routes,
        RootDescriptor::Staged(_) => return Err(invalid(&root, "pending adoption")),
    };
    let order = participants
        .iter()
        .enumerate()
        .map(|(index, p)| (p.mutation.name.clone(), index))
        .collect::<BTreeMap<_, _>>();
    if order.len() != participants.len() {
        return Err(invalid(&root, "duplicate participant"));
    }
    let mut identities = std::collections::BTreeSet::new();
    for (name, index) in &order {
        let p = participants
            .get(*index)
            .ok_or_else(|| invalid(&root, "participant index"))?;
        if !name_valid(name) || p.mutation.options.access_mode != AccessMode::ReadWrite {
            return Err(invalid(&root, "invalid or read-only participant"));
        }
        let logical = root.join(name);
        if std::fs::symlink_metadata(&logical)
            .map_err(|e| io(&logical, e))?
            .file_type()
            .is_symlink()
        {
            return Err(invalid(
                &logical,
                "namespace links are not transaction participants",
            ));
        }
        let selected = std::fs::canonicalize(resolve(&logical)?).map_err(|e| io(&logical, e))?;
        let directory = std::fs::canonicalize(&p.store.directory).map_err(|e| io(&logical, e))?;
        if selected != directory || !identities.insert(directory) {
            return Err(invalid(&logical, "foreign or aliased namespace handle"));
        }
    }
    // Match the existing maintenance -> state -> writer -> WAL -> active order.
    // Each level is acquired in canonical namespace order, including reversed inputs.
    let ordered = order
        .values()
        .map(|index| {
            participants
                .get(*index)
                .ok_or_else(|| invalid(&root, "participant index"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut maintenance = Vec::new();
    let mut states = Vec::new();
    let mut owners = Vec::new();
    let mut wals = Vec::new();
    let mut actives = Vec::new();
    for p in &ordered {
        maintenance.push(
            p.store
                .maintenance
                .lock()
                .map_err(|_| invalid(&root, "maintenance lock"))?,
        );
    }
    for p in &ordered {
        let state = p
            .store
            .state
            .lock()
            .map_err(|_| invalid(&root, "state lock"))?;
        match *state {
            super::StoreState::Open => {}
            super::StoreState::Closing => return Err(StoreError::Closing),
            super::StoreState::Closed => return Err(StoreError::Closed),
        }
        states.push(state);
    }
    for p in &ordered {
        let owner = p
            .store
            .writer_lock
            .lock()
            .map_err(|_| invalid(&root, "writer lock"))?;
        if owner.is_none() {
            return Err(StoreError::ReadOnly);
        }
        owners.push(owner);
    }
    for p in &ordered {
        wals.push(
            p.store
                .wal_writer
                .lock()
                .map_err(|_| invalid(&root, "WAL lock"))?,
        );
    }
    for p in &ordered {
        actives.push(
            p.store
                .active
                .lock()
                .map_err(|_| invalid(&root, "active lock"))?,
        );
    }
    let mut source_snapshots = Vec::new();
    // All declarations and mutations validate before any WAL append/publication.
    for ((p, wal), active) in ordered.iter().zip(&wals).zip(&actives) {
        wal.as_ref().ok_or(StoreError::ReadOnly)?;
        if read_optional_vfs(
            p.store.vfs.as_ref(),
            &p.store.directory.join(crate::ingest::PURGE_INTENT_FILE),
        )?
        .is_some()
        {
            return Err(StoreError::StoreBusy {
                path: p.store.directory.clone(),
            });
        }
        active.as_ref().ok_or(StoreError::Closed)?;
        if !p.mutation.deletes.is_empty() || p.mutation.delete_where.is_some() {
            p.store.require_no_snapshot_views()?;
        }
        let snapshot = p
            .store
            .snapshot
            .read()
            .map_err(|_| invalid(&root, "snapshot lock"))?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        super::resolve_open_schema(
            true,
            &snapshot,
            p.mutation.options.schema.as_ref(),
            AccessMode::ReadOnly,
        )?;
        super::validate_epoch_identity(
            p.store.epoch_identity(),
            p.mutation
                .options
                .epoch
                .as_ref()
                .map(crate::epoch::StoreEpoch::identity),
            true,
            AccessMode::ReadOnly,
        )?;
        let tokenizer = crate::fts::tokenizer::Analyzer::new(
            p.mutation
                .options
                .tokenizer
                .clone()
                .unwrap_or_else(crate::fts::tokenizer::TokenizerConfig::text_default),
        )
        .map_err(StoreError::Tokenizer)?;
        if tokenizer.config() != p.store.tokenizer.config() {
            return Err(invalid(&root, "participant tokenizer mismatch"));
        }
        source_snapshots.push(snapshot);
    }
    let mut staged = Vec::new();
    for (((p, wal), active), snapshot) in ordered
        .iter()
        .zip(&wals)
        .zip(&actives)
        .zip(&source_snapshots)
    {
        let writer = wal.as_ref().ok_or(StoreError::ReadOnly)?;
        let current = active.as_ref().ok_or(StoreError::Closed)?;
        match p
            .store
            .stage_namespace(&p.mutation, current, snapshot, writer.durable_end())
        {
            Ok(stage) => staged.push(stage),
            Err(error) => {
                for (prior, stage) in ordered.iter().zip(staged) {
                    cleanup_stage(prior.store, stage, None)?;
                }
                return Err(invalid(&p.store.directory, &error.to_string()));
            }
        }
    }
    let transaction = (u128::from(std::process::id()) << 96)
        | (u128::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| invalid(&root, "transaction clock"))?
                .as_nanos() as u64,
        ) << 32)
        | u128::from(NEXT.fetch_add(1, Ordering::Relaxed));
    let mut descriptor = StagedDescriptor(BTreeMap::new(), existing_routes);
    let mut lengths = Vec::new();
    let mut selections = Vec::new();
    for (p, stage) in ordered.iter().zip(&staged) {
        let wal_path = p.store.directory.join("wal.ze");
        lengths.push(p.store.vfs.open(&wal_path).map_err(|e| io(&wal_path, e))?);
        if stage.records.is_empty() {
            selections.push(None);
            continue;
        }
        let bytes =
            crate::manifest::encode_manifest(&stage.manifest).map_err(StoreError::Manifest)?;
        let first_seq = wals
            .get(selections.len())
            .and_then(|wal| wal.as_ref())
            .ok_or(StoreError::ReadOnly)?
            .durable_end()
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let binding = TransactionBinding {
            transaction,
            participant: match root_id {
                Some(id) => portable::participant_id(id, &p.mutation.name)?,
                None => participant_identity(&root, &p.mutation.name)?,
            },
            first_seq,
            last_seq: first_seq
                .checked_add(stage.records.len() as u64 - 1)
                .ok_or(StoreError::GenerationOverflow)?,
            manifest_digest: xxh3_64(&bytes),
            final_generation: stage.active.generation,
        };
        let selection = StagedSelection {
            manifest: format!(".ze-manifest-{transaction}"),
            binding,
        };
        descriptor
            .0
            .insert(p.mutation.name.clone(), selection.clone());
        selections.push(Some(selection));
    }
    if descriptor.0.is_empty() {
        let generations = ordered
            .iter()
            .zip(&staged)
            .map(|(p, s)| (p.mutation.name.clone(), s.active.generation))
            .collect::<BTreeMap<_, _>>();
        return participants
            .iter()
            .map(|p| {
                generations
                    .get(&p.mutation.name)
                    .copied()
                    .ok_or_else(|| invalid(&root, "missing generation"))
            })
            .collect();
    }
    let decision = encode_root_record(root_id, &RootDescriptor::Staged(descriptor.clone()))?;
    let prepared = (|| {
        if root_id.is_some() {
            if read_optional_vfs(vfs, &root.join(RECORD))?.is_none() {
                publish(
                    vfs,
                    &root,
                    &encode_root_record(root_id, &RootDescriptor::Legacy(Routes::new()))?,
                    step,
                )?;
                step("bootstrap root published").map_err(|e| io(&root, e))?;
            }
            for p in &ordered {
                let logical = root.join(&p.mutation.name);
                let reference = logical.join(REFERENCE);
                if read_optional_vfs(vfs, &reference)?.is_none() {
                    publish_reference(
                        vfs,
                        &logical,
                        &encode_participant_reference(root_id, &root, &p.mutation.name, 1)?,
                        step,
                    )?;
                    step("participant reference installed").map_err(|e| io(&logical, e))?;
                }
            }
        }
        for (((p, stage), selection), wal) in ordered
            .iter()
            .zip(&mut staged)
            .zip(&selections)
            .zip(&mut wals)
        {
            let Some(selection) = selection else {
                continue;
            };
            let bytes =
                crate::manifest::encode_manifest(&stage.manifest).map_err(StoreError::Manifest)?;
            durable_write(
                p.store.vfs.as_ref(),
                &p.store.directory.join(&selection.manifest),
                &bytes,
                step,
            )?;
            sync_dir(p.store.vfs.as_ref(), &p.store.directory, step)?;
            let count = u32::try_from(stage.records.len())
                .map_err(|_| invalid(&root, "prepared member count"))?;
            let frames = stage
                .records
                .iter()
                .zip(0_u32..)
                .map(|((op, payload), index)| {
                    wal_payload::encode_prepared(selection.binding, index, count, *op, payload)
                        .map_err(|e| invalid(&root, &e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let refs = frames
                .iter()
                .map(|frame| (wal_payload::PREPARED_MUTATION_V1, frame.as_slice()))
                .collect::<Vec<_>>();
            wal.as_mut()
                .ok_or(StoreError::ReadOnly)?
                .commit_many(&refs)?;
            step("prepared append").map_err(|e| io(&p.store.directory, e))?;
            p.store
                .vfs
                .sync(&p.store.directory.join("wal.ze"), SyncKind::Full)
                .map_err(|e| io(&p.store.directory, e))?;
            step("prepared WAL sync").map_err(|e| io(&p.store.directory, e))?;
            let active = Arc::get_mut(&mut stage.active.segment)
                .ok_or_else(|| invalid(&root, "private active ownership"))?;
            for (row, seq) in stage.rows.iter().zip(selection.binding.first_seq..) {
                for row in row {
                    active.set_sequence(*row, crate::wal::LogSeq::new(seq))?;
                }
            }
        }
        if root_id.is_none() {
            if read_optional_vfs(vfs, &root.join(RECORD))?.is_none() {
                publish(
                    vfs,
                    &root,
                    &encode_root_record(root_id, &RootDescriptor::Legacy(Routes::new()))?,
                    step,
                )?;
            }
            for p in &ordered {
                let logical = root.join(&p.mutation.name);
                let reference = logical.join(REFERENCE);
                if read_optional_vfs(vfs, &reference)?.is_none() {
                    durable_write(
                        vfs,
                        &reference,
                        &encode_participant_reference(root_id, &root, &p.mutation.name, 1)?,
                        step,
                    )?;
                    sync_dir(vfs, &logical, step)?;
                }
            }
        }
        Ok::<_, StoreError>(())
    })();
    if let Err(error) = prepared {
        rollback_stages(&ordered, staged, &selections, &lengths, &mut wals)?;
        return Err(error);
    }
    let mut snapshots = Vec::new();
    for p in &ordered {
        match p.store.snapshot.write() {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(_) => {
                rollback_stages(&ordered, staged, &selections, &lengths, &mut wals)?;
                return Err(invalid(&root, "snapshot lock"));
            }
        }
    }
    let mut publication_attempted = false;
    let published = (|| {
        let temporary = root.join(".ze-namespaces.tmp");
        durable_write(vfs, &temporary, &decision, step)?;
        publication_attempted = true;
        vfs.rename(&temporary, &root.join(RECORD))
            .map_err(|e| io(&root, e))?;
        step("commit rename").map_err(|e| io(&root, e))?;
        sync_dir(vfs, &root, step)
    })();
    if let Err(error) = published {
        if publication_attempted {
            for wal in &mut wals {
                **wal = None;
            }
        } else {
            rollback_stages(&ordered, staged, &selections, &lengths, &mut wals)?;
        }
        return Err(error);
    }
    // Once publication is attempted its outcome may be indeterminate. Never
    // truncate prepared evidence in this branch; fence queued writes instead.
    let adopted = (|| {
        for ((active, stage), snapshot) in actives.iter_mut().zip(&staged).zip(&mut snapshots) {
            **active = Some(crate::ingest::ActiveState {
                generation: stage.active.generation,
                segment: Arc::clone(&stage.active.segment),
            });
            **snapshot = Some(Arc::clone(&stage.snapshot));
        }
        step("live states installed").map_err(|e| io(&root, e))?;
        for ((p, stage), selection) in ordered.iter().zip(&staged).zip(&selections) {
            if let Some(selection) = selection {
                accept(
                    p.store.vfs.as_ref(),
                    &p.store.directory,
                    selection,
                    &stage.manifest,
                    step,
                )?;
            }
        }
        publish(
            vfs,
            &root,
            &encode_root_record(root_id, &RootDescriptor::Legacy(descriptor.1.clone()))?,
            step,
        )
    })();
    if let Err(error) = adopted {
        for wal in &mut wals {
            **wal = None;
        }
        return Err(error);
    }
    drop(snapshots);
    drop(actives);
    let mut completed = BTreeMap::new();
    for ((p, stage), wal) in ordered.iter().zip(&staged).zip(&mut wals) {
        let generation = if read_optional_vfs(
            p.store.vfs.as_ref(),
            &p.store.directory.join(crate::ingest::PURGE_INTENT_FILE),
        )?
        .is_some()
        {
            match p
                .store
                .complete_namespace_purge_locked(wal.as_mut().ok_or(StoreError::ReadOnly)?)
            {
                Ok(generation) => generation,
                Err(error) => {
                    for wal in &mut wals {
                        **wal = None;
                    }
                    return Err(StoreError::PurgeRecovery {
                        detail: error.to_string(),
                    });
                }
            }
        } else {
            stage.active.generation
        };
        completed.insert(p.mutation.name.clone(), generation);
    }
    if deleting {
        reclamation::run(vfs, &root, step)?;
        reclamation::require_retired_erased(vfs, &root)?;
    }
    let by_name = completed;
    drop(wals);
    drop(owners);
    drop(states);
    drop(maintenance);
    drop(ordered);
    participants
        .iter_mut()
        .map(|p| {
            by_name
                .get(&p.mutation.name)
                .copied()
                .ok_or_else(|| invalid(&root, "missing generation"))
        })
        .collect()
}

fn cleanup_stage(
    store: &Store,
    stage: crate::ingest::NamespaceStage,
    selection: Option<&StagedSelection>,
) -> Result<(), StoreError> {
    let mut paths = stage
        .replacements
        .iter()
        .map(|id| store.directory.join(id.file_name()))
        .collect::<Vec<_>>();
    if let Some(selection) = selection {
        paths.push(store.directory.join(&selection.manifest));
    }
    for path in paths {
        match store.vfs.delete(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&path, e)),
        }
    }
    store
        .vfs
        .sync(&store.directory, SyncKind::Full)
        .map_err(|e| io(&store.directory, e))
}

pub(super) fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}
pub(crate) fn invalid(path: &Path, message: &str) -> StoreError {
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
#[derive(Clone, Debug, Eq, PartialEq)]
enum RootDescriptor {
    Legacy(Routes),
    Staged(StagedDescriptor),
}
fn routes(root: &Path) -> Result<Routes, StoreError> {
    match root_descriptor_vfs(&StdVfs, root)? {
        RootDescriptor::Legacy(routes) => Ok(routes),
        RootDescriptor::Staged(_) => Err(invalid(
            &root.join(RECORD),
            "staged manifest requires transaction adoption",
        )),
    }
}
fn root_descriptor(root: &Path) -> Result<RootDescriptor, StoreError> {
    root_descriptor_vfs(&StdVfs, root)
}
fn root_descriptor_vfs(vfs: &dyn Vfs, root: &Path) -> Result<RootDescriptor, StoreError> {
    root_record_vfs(vfs, root).map(|(_, descriptor)| descriptor)
}
fn encode_root_record(
    id: Option<NamespaceRootId>,
    descriptor: &RootDescriptor,
) -> Result<Vec<u8>, StoreError> {
    match id {
        Some(id) => portable::encode_root(id, descriptor),
        None => match descriptor {
            RootDescriptor::Legacy(routes) => Ok(encode(routes)),
            RootDescriptor::Staged(staged) => encode_staged(staged),
        },
    }
}

fn encode_participant_reference(
    id: Option<NamespaceRootId>,
    root: &Path,
    name: &str,
    parent_depth: u8,
) -> Result<Vec<u8>, StoreError> {
    match id {
        Some(root_id) => portable::encode_reference(&portable::Reference {
            root_id,
            parent_depth,
            name: name.to_owned(),
        }),
        None => Ok(envelope(
            root.to_str()
                .ok_or_else(|| invalid(root, "root UTF-8"))?
                .as_bytes(),
        )),
    }
}

fn root_record_vfs(
    vfs: &dyn Vfs,
    root: &Path,
) -> Result<(Option<NamespaceRootId>, RootDescriptor), StoreError> {
    let path = root.join(RECORD);
    let Some(bytes) = read_optional_vfs(vfs, &path)? else {
        return Ok((None, RootDescriptor::Legacy(Routes::new())));
    };
    if bytes.starts_with(portable::ROOT_MAGIC) {
        return portable::decode_root(&path, &bytes).map(|(id, descriptor)| (Some(id), descriptor));
    }
    decode_descriptor(&path, &bytes).map(|descriptor| (None, descriptor))
}
fn decode_descriptor(path: &Path, bytes: &[u8]) -> Result<RootDescriptor, StoreError> {
    if bytes.starts_with(STAGED_MAGIC) {
        return decode_staged(path, bytes).map(RootDescriptor::Staged);
    }
    decode_routes(path, bytes).map(RootDescriptor::Legacy)
}
fn decode_routes(path: &Path, bytes: &[u8]) -> Result<Routes, StoreError> {
    let text = std::str::from_utf8(body(path, bytes)?)
        .map_err(|_| invalid(path, "namespace record UTF-8"))?;
    let mut result = Routes::new();
    for line in text.split_terminator('\n') {
        let (name, destination) = line
            .split_once('\t')
            .ok_or_else(|| invalid(path, "namespace route"))?;
        let (transaction, child) = destination
            .split_once('/')
            .ok_or_else(|| invalid(path, "namespace destination"))?;
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
            return Err(invalid(path, "invalid or duplicate namespace route"));
        }
    }
    if encode(&result) != bytes {
        return Err(invalid(path, "noncanonical namespace record"));
    }
    Ok(result)
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
    let portable = portable::authority(&StdVfs, path)?;
    let parent = portable.map_or(parent, |(root, _, _)| root);
    let reference = if portable.is_none() {
        read_optional(&path.join(REFERENCE))?
    } else {
        None
    };
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
        RootDescriptor::Staged(mut descriptor) => descriptor.1.remove(name),
    };
    match selected {
        None => Ok(path.to_path_buf()),
        Some(destination) => {
            let selected = parent.join(&destination);
            if let Some((_, _, id)) = portable
                && portable::authority(&StdVfs, &selected)?.map(|(_, _, selected_id)| selected_id)
                    != Some(id)
            {
                return Err(invalid(&selected, "foreign portable route participant"));
            }
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
// The final reference is always absent or complete, including during process death.
fn publish_reference(
    vfs: &dyn Vfs,
    directory: &Path,
    bytes: &[u8],
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    let temporary = directory.join(".ze-namespace-root.tmp");
    let reference = directory.join(REFERENCE);
    durable_write(vfs, &temporary, bytes, step)?;
    vfs.rename(&temporary, &reference)
        .map_err(|e| io(&reference, e))?;
    step("reference rename").map_err(|e| io(&reference, e))?;
    sync_dir(vfs, directory, step)
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
        let acceptance = name
            .strip_prefix(".ze-accepted-")
            .is_some_and(|transaction| transaction.parse::<u128>().is_ok());
        if name != "manifest.ze" && name != "wal.ze" && !name.ends_with(".zseg") && !acceptance {
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

/// Atomically commits ordered namespace mutations through one root rename.
///
/// This path API opens closed writers and shares the incremental live protocol.
/// Use [`namespace_batch_live`] when writable handles are already open. Data,
/// decision and local acceptance are fully synced before success is returned.
/// An error once publication is attempted is indeterminate; reopen before
/// retrying. Deleted bytes and superseded artifacts are retained in this scope.
/// Generations follow input order. Ordinary writes perform no root I/O.
pub fn namespace_batch(
    root: &Path,
    mutations: Vec<NamespaceMutation>,
) -> Result<Vec<u64>, StoreError> {
    execute(root, mutations, Operation::Batch, &mut |_| Ok(()))
}

/// Protocol interruption seam for kill/fault tests; never enabled by environment.
#[cfg(any(test, feature = "test-seams"))]
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
#[cfg(any(test, feature = "test-seams"))]
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
    reclamation::run(&StdVfs, &root, step)?;
    if root_record_vfs(&StdVfs, &root)?.0.is_some() {
        normalize_accepted(&StdVfs, &root, step)?;
    }
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
        Operation::Batch => {
            let live = mutations
                .into_iter()
                .map(|mutation| {
                    sources
                        .get(&mutation.name)
                        .map(|store| LiveNamespaceMutation { store, mutation })
                        .ok_or_else(|| invalid(&root, "missing source"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            return execute_live(&root, live, &StdVfs, step, Some(_coordinator));
        }
    }
    let mut root_id = root_record_vfs(&StdVfs, &root)?.0;
    if read_optional(&root.join(RECORD))?.is_none() {
        root_id = Some(NamespaceRootId::generate().map_err(|e| io(&root, e))?);
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
    let intent = encode(&next);
    let decision = encode_root_record(root_id, &RootDescriptor::Legacy(next.clone()))?;
    if decision.len() > MAX_RECORD {
        return Err(invalid(&root, "namespace decision too large"));
    }
    durable_write(&StdVfs, &transaction.join("intent.ze"), &intent, step)?;
    sync_dir(&StdVfs, &transaction, step)?;
    sync_dir(&StdVfs, &root, step)?;
    if root_id.is_some() {
        if read_optional(&root.join(RECORD))?.is_none() {
            publish(
                &StdVfs,
                &root,
                &encode_root_record(root_id, &RootDescriptor::Legacy(Routes::new()))?,
                step,
            )?;
            step("bootstrap root published").map_err(|e| io(&root, e))?;
        }
        for name in ordered.keys() {
            let logical = root.join(name);
            if read_optional(&logical.join(REFERENCE))?.is_none() {
                publish_reference(
                    &StdVfs,
                    &logical,
                    &encode_participant_reference(root_id, &root, name, 1)?,
                    step,
                )?;
                step("participant reference installed").map_err(|e| io(&logical, e))?;
            }
        }
    }
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
        let store = match root_id {
            Some(id) => {
                publish_reference(
                    &StdVfs,
                    &destination,
                    &encode_participant_reference(root_id, &root, &mutation.name, 2)?,
                    step,
                )?;
                let authority =
                    PrivatePreparation::new(id, &mutation.name, &destination, &_coordinator);
                Store::open_private_preparation(&destination, options, authority)?
            }
            None => Store::open(&destination, options)?,
        };
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
    if root_id.is_none() {
        // Create the empty baseline before references, so a crash while enlisting
        // namespaces still resolves to all original stores.
        if read_optional(&root.join(RECORD))?.is_none() {
            publish(
                &StdVfs,
                &root,
                &encode_root_record(root_id, &RootDescriptor::Legacy(Routes::new()))?,
                step,
            )?;
        }
        for name in ordered.keys() {
            let directory = root.join(name);
            let reference = directory.join(REFERENCE);
            if read_optional(&reference)?.is_none() {
                let root_text = root
                    .to_str()
                    .ok_or_else(|| invalid(&root, "root must be UTF-8"))?;
                publish_reference(&StdVfs, &directory, &envelope(root_text.as_bytes()), step)?;
            }
        }
    }
    publish(&StdVfs, &root, &decision, step)?;
    drop(sources);
    reclamation::run(&StdVfs, &root, step)?;
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
    fn nonparticipant_route_survives_v2_publish() {
        let temporary = tempfile::tempdir().expect("root");
        let root = temporary.path();
        let destination = ".ze-batch-1/c";
        let selected = root.join(destination);
        std::fs::create_dir_all(&selected).expect("selected directory");
        std::fs::create_dir(root.join("c")).expect("logical directory");
        let routes = Routes::from([("c".into(), destination.into())]);
        std::fs::write(root.join(".ze-batch-1/intent.ze"), encode(&routes)).expect("intent");
        std::fs::write(selected.join(PREPARED), envelope(destination.as_bytes()))
            .expect("prepared marker");
        for filename in ["manifest.ze", "wal.ze"] {
            std::fs::write(selected.join(filename), b"retained").expect("payload");
        }
        publish(&StdVfs, root, &encode(&routes), &mut |_| Ok(())).expect("legacy publish");
        assert_eq!(resolve(&root.join("c")).expect("legacy resolve"), selected);
        let descriptor = StagedDescriptor(
            BTreeMap::from([(
                "a".into(),
                StagedSelection {
                    manifest: ".ze-manifest-1".into(),
                    binding: crate::ingest::wal_payload::TransactionBinding {
                        transaction: 1,
                        participant: 2,
                        first_seq: 3,
                        last_seq: 3,
                        manifest_digest: 4,
                        final_generation: 5,
                    },
                },
            )]),
            routes,
        );
        publish(
            &StdVfs,
            root,
            &encode_staged(&descriptor).expect("v2"),
            &mut |_| Ok(()),
        )
        .expect("v2 publish");
        assert_eq!(resolve(&root.join("c")).expect("v2 resolve"), selected);
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn staged_descriptor_round_trips_and_old_reader_rejects() {
        let binding = crate::ingest::wal_payload::TransactionBinding {
            transaction: 1,
            participant: 2,
            first_seq: 3,
            last_seq: 3,
            manifest_digest: 4,
            final_generation: 5,
        };
        let descriptor = StagedDescriptor(
            BTreeMap::from([(
                "a".into(),
                StagedSelection {
                    manifest: ".ze-manifest-1".into(),
                    binding,
                },
            )]),
            Routes::new(),
        );
        let bytes = encode_staged(&descriptor).expect("encode");
        assert_eq!(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "5a454e5330303032010000000100610e002e7a652d6d616e69666573742d3101000000000000000000000000000000020000000000000000000000000000000300000000000000030000000000000004000000000000000500000000000000100000005a454e5330303031a3d24bff963b33d2986d4df50a4f2e73"
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

// ZE-256 v2 carries staged selections and the complete existing route table.
// V1 readers reject the distinct magic before interpreting any route.
const STAGED_MAGIC: &[u8] = b"ZENS0002";
#[derive(Clone, Debug, Eq, PartialEq)]
struct StagedSelection {
    manifest: String,
    binding: crate::ingest::wal_payload::TransactionBinding,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct StagedDescriptor(BTreeMap<String, StagedSelection>, Routes);

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
    let routes = encode(&descriptor.1);
    let length = u32::try_from(routes.len()).map_err(|_| invalid(path, "route table length"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&routes);
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
    let route_length = u32::from_le_bytes(
        take(path, &mut remaining, 4)?
            .try_into()
            .map_err(|_| invalid(path, "route table length"))?,
    );
    let route_length =
        usize::try_from(route_length).map_err(|_| invalid(path, "route table length"))?;
    let routes = decode_routes(path, take(path, &mut remaining, route_length)?)?;
    let descriptor = StagedDescriptor(result, routes);
    if !remaining.is_empty() || encode_staged(&descriptor)? != bytes {
        return Err(invalid(path, "noncanonical staged decision"));
    }
    Ok(descriptor)
}

fn read_optional_vfs(vfs: &dyn Vfs, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match vfs.open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
        Ok(n) if n > MAX_RECORD as u64 => Err(invalid(path, "namespace record too large")),
        Ok(_) => vfs.read(path).map(Some).map_err(|e| io(path, e)),
    }
}

fn location(directory: &Path) -> Result<(&Path, &str), StoreError> {
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid(directory, "participant name"))?;
    let parent = directory
        .parent()
        .ok_or_else(|| invalid(directory, "participant root"))?;
    let root = if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".ze-batch-"))
    {
        parent
            .parent()
            .ok_or_else(|| invalid(directory, "route root"))?
    } else {
        parent
    };
    Ok((root, name))
}
fn reader_location<'a>(
    vfs: &dyn Vfs,
    directory: &'a Path,
    preparation: Option<&PrivatePreparation>,
) -> Result<(&'a Path, &'a str, Option<NamespaceRootId>), StoreError> {
    if let Some((root, name, id)) =
        portable::authority_with_preparation(vfs, directory, preparation)?
    {
        return Ok((root, name, Some(id)));
    }
    location(directory).map(|(root, name)| (root, name, None))
}
fn reader_participant_identity(
    vfs: &dyn Vfs,
    directory: &Path,
    preparation: Option<&PrivatePreparation>,
) -> Result<u128, StoreError> {
    let (root, name, id) = reader_location(vfs, directory, preparation)?;
    match id {
        Some(id) => portable::participant_id(id, name),
        None => participant_identity(root, name),
    }
}
fn acceptance_path(
    directory: &Path,
    binding: crate::ingest::wal_payload::TransactionBinding,
) -> PathBuf {
    directory.join(format!(".ze-accepted-{}", binding.transaction))
}
fn accepted(
    vfs: &dyn Vfs,
    directory: &Path,
    binding: crate::ingest::wal_payload::TransactionBinding,
) -> Result<bool, StoreError> {
    let path = acceptance_path(directory, binding);
    let Some(bytes) = read_optional_vfs(vfs, &path)? else {
        return Ok(false);
    };
    let recorded = crate::ingest::wal_payload::TransactionBinding::decode(body(&path, &bytes)?)
        .map_err(|e| invalid(&path, &e.to_string()))?;
    if recorded != binding {
        return Err(invalid(&path, "local acceptance binding mismatch"));
    }
    Ok(true)
}
// The committed root authorizes purges before local acceptance is written.
// Local acceptance retains that authority after root retirement.
pub(crate) fn owns_purge_obligation(
    vfs: &dyn Vfs,
    directory: &Path,
    token: u64,
    preparation: Option<&PrivatePreparation>,
) -> Result<bool, StoreError> {
    for path in vfs.list(directory).map_err(|e| io(directory, e))? {
        let Some(transaction) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(".ze-accepted-"))
            .and_then(|name| name.parse::<u128>().ok())
        else {
            continue;
        };
        if xxh3_64(&transaction.to_le_bytes()) != token {
            continue;
        }
        let bytes = vfs.read(&path).map_err(|e| io(&path, e))?;
        let binding = crate::ingest::wal_payload::TransactionBinding::decode(body(&path, &bytes)?)
            .map_err(|e| invalid(&path, &e.to_string()))?;
        if binding.transaction != transaction {
            return Err(invalid(&path, "purge acceptance transaction mismatch"));
        }
        return Ok(true);
    }
    if preparation.is_none()
        && let Some(selected) = selection(vfs, directory)?
        && xxh3_64(&selected.binding.transaction.to_le_bytes()) == token
    {
        staged_manifest(vfs, directory, &selected)?;
        return Ok(true);
    }
    Ok(false)
}

fn selection(vfs: &dyn Vfs, directory: &Path) -> Result<Option<StagedSelection>, StoreError> {
    let (root, name, _) = reader_location(vfs, directory, None)?;
    let RootDescriptor::Staged(mut descriptor) = root_descriptor_vfs(vfs, root)? else {
        return Ok(None);
    };
    let Some(selected) = descriptor.0.remove(name) else {
        return Ok(None);
    };
    let identity = reader_participant_identity(vfs, directory, None)?;
    if selected.binding.participant != identity {
        return Err(invalid(directory, "participant identity mismatch"));
    }
    Ok(Some(selected))
}
fn selected_manifest(
    vfs: &dyn Vfs,
    directory: &Path,
    selected: &StagedSelection,
) -> Result<crate::manifest::Manifest, StoreError> {
    let path = directory.join(&selected.manifest);
    let bytes = vfs.read(&path).map_err(|e| io(&path, e))?;
    if xxh3_64(&bytes) != selected.binding.manifest_digest {
        return Err(invalid(&path, "staged manifest digest mismatch"));
    }
    let manifest = crate::manifest::decode_manifest(&path.display().to_string(), &bytes)
        .map_err(StoreError::Manifest)?;
    if manifest.generation != selected.binding.final_generation {
        return Err(invalid(&path, "staged generation mismatch"));
    }
    Ok(manifest)
}

fn staged_manifest(
    vfs: &dyn Vfs,
    directory: &Path,
    selected: &StagedSelection,
) -> Result<crate::manifest::Manifest, StoreError> {
    let manifest = selected_manifest(vfs, directory, selected)?;
    let path = directory.join(&selected.manifest);
    let wal = crate::wal::WalReader::open(vfs, &directory.join("wal.ze"))
        .map_err(StoreError::Wal)?
        .into_clean()
        .map_err(StoreError::WalRecovery)?;
    // Exact complete gated range is necessary before any canonical adoption.
    let selected_records = wal
        .records()
        .iter()
        .filter(|r| (selected.binding.first_seq..=selected.binding.last_seq).contains(&r.seq.get()))
        .collect::<Vec<_>>();
    let member_count = selected
        .binding
        .last_seq
        .checked_sub(selected.binding.first_seq)
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| invalid(&path, "invalid committed prepared range"))?;
    if selected_records.len() as u64 != member_count {
        return Err(invalid(&path, "committed prepared range incomplete"));
    }
    for (record, index) in selected_records.iter().zip(0_u32..) {
        let payload = record
            .payload()
            .map_err(|e| invalid(&path, &e.to_string()))?;
        let member = crate::ingest::wal_payload::decode_prepared(payload)
            .map_err(|e| invalid(&path, &e.to_string()))?;
        if record.op != crate::ingest::wal_payload::PREPARED_MUTATION_V1
            || member.binding != selected.binding
            || member.index != index
        {
            return Err(invalid(&path, "committed prepared binding mismatch"));
        }
    }
    Ok(manifest)
}

pub(super) fn manifest_for_open(
    vfs: &dyn Vfs,
    directory: &Path,
    preparation: Option<&PrivatePreparation>,
) -> Result<PathBuf, StoreError> {
    let canonical = directory.join("manifest.ze");
    if vfs
        .open(&canonical)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        && vfs
            .list(directory)
            .map_err(|e| io(directory, e))?
            .iter()
            .any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".ze-accepted-"))
            })
    {
        return Err(invalid(&canonical, "missing locally accepted manifest"));
    }
    if preparation.is_some() {
        portable::authority_with_preparation(vfs, directory, preparation)?;
        return Ok(canonical);
    }
    match selection(vfs, directory)? {
        Some(selected) if !accepted(vfs, directory, selected.binding)? => {
            staged_manifest(vfs, directory, &selected)?;
            Ok(directory.join(selected.manifest))
        }
        _ => Ok(directory.join("manifest.ze")),
    }
}
fn accept(
    vfs: &dyn Vfs,
    directory: &Path,
    selected: &StagedSelection,
    manifest: &crate::manifest::Manifest,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    // These frames were synced before the root commit and validated against
    // its binding. Derive obligations only for this committed transaction.
    let wal = crate::wal::WalReader::open(vfs, &directory.join("wal.ze"))
        .map_err(StoreError::Wal)?
        .into_clean()
        .map_err(StoreError::WalRecovery)?;
    let mut ids = Vec::new();
    for record in wal.records().iter().filter(|record| {
        (selected.binding.first_seq..=selected.binding.last_seq).contains(&record.seq.get())
    }) {
        let payload = record
            .payload()
            .map_err(|e| invalid(directory, &e.to_string()))?;
        let member = crate::ingest::wal_payload::decode_prepared(payload)
            .map_err(|e| invalid(directory, &e.to_string()))?;
        if member.binding != selected.binding {
            return Err(invalid(directory, "purge binding mismatch"));
        }
        if let crate::ingest::wal_payload::MutationPayload::Delete(deleted) = member.mutation {
            ids.extend(deleted);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    crate::ingest::namespace_purge_intent(vfs, directory, selected.binding.transaction, ids)
        .map_err(|e| StoreError::PurgeRecovery {
            detail: e.to_string(),
        })?;
    step("purge obligation adopted").map_err(|e| io(directory, e))?;
    let bytes = crate::manifest::encode_manifest(manifest).map_err(StoreError::Manifest)?;
    let temporary = directory.join(".manifest.ze.tmp");
    durable_write(vfs, &temporary, &bytes, step)?;
    vfs.rename(&temporary, &directory.join("manifest.ze"))
        .map_err(|e| io(directory, e))?;
    step("accept manifest rename").map_err(|e| io(directory, e))?;
    sync_dir(vfs, directory, step)?;
    let binding = selected
        .binding
        .encode()
        .map_err(|e| invalid(directory, &e.to_string()))?;
    let committed = acceptance_path(directory, selected.binding);
    let temporary = committed.with_extension("tmp");
    durable_write(vfs, &temporary, &envelope(&binding), step)?;
    vfs.rename(&temporary, &committed)
        .map_err(|e| io(&committed, e))?;
    step("accept binding rename").map_err(|e| io(&committed, e))?;
    sync_dir(vfs, directory, step)
}

pub(super) fn adopt_for_open(vfs: &dyn Vfs, directory: &Path) -> Result<(), StoreError> {
    if let Some(selected) = selection(vfs, directory)? {
        let (root, _, _) = reader_location(vfs, directory, None)?;
        // A surviving rename is not proof that its directory entry was synced.
        // Persist both authorities before admitting even a derived-mode writer.
        vfs.sync(root, SyncKind::Full).map_err(|e| io(root, e))?;
        if accepted(vfs, directory, selected.binding)? {
            vfs.sync(directory, SyncKind::Full)
                .map_err(|e| io(directory, e))?;
        } else {
            let manifest = staged_manifest(vfs, directory, &selected)?;
            accept(vfs, directory, &selected, &manifest, &mut |_| Ok(()))?;
        }
    }
    Ok(())
}
fn normalize_accepted(
    vfs: &dyn Vfs,
    root: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    let (root_id, descriptor) = root_record_vfs(vfs, root)?;
    let RootDescriptor::Staged(descriptor) = descriptor else {
        return Ok(());
    };
    for (name, selected) in &descriptor.0 {
        let directory = root.join(descriptor.1.get(name).map_or(name.as_str(), String::as_str));
        if !accepted(vfs, &directory, selected.binding)? {
            return Err(StoreError::StoreBusy { path: directory });
        }
        sync_dir(vfs, &directory, step)?;
    }
    publish(
        vfs,
        root,
        &encode_root_record(root_id, &RootDescriptor::Legacy(descriptor.1))?,
        step,
    )
}

pub(crate) fn transaction_decisions(
    vfs: &dyn Vfs,
    directory: &Path,
    records: &[crate::wal::VisibleRecord],
    absorbed: u64,
    preparation: Option<&PrivatePreparation>,
) -> Result<BTreeMap<u128, crate::ingest::wal_payload::TransactionBinding>, StoreError> {
    let mut decisions = BTreeMap::new();
    let mut checked = std::collections::BTreeSet::new();
    for record in records.iter().filter(|r| {
        r.seq.get() > absorbed && r.op == crate::ingest::wal_payload::PREPARED_MUTATION_V1
    }) {
        let payload = record.payload().map_err(|source| StoreError::WalRecord {
            seq: record.seq,
            source,
        })?;
        let member = crate::ingest::wal_payload::decode_prepared(payload).map_err(|source| {
            StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source,
            }
        })?;
        if !checked.insert(member.binding.transaction) {
            continue;
        }
        let identity = reader_participant_identity(vfs, directory, preparation)?;
        if member.binding.participant != identity {
            return Err(StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source: crate::ingest::wal_payload::PayloadError::TransactionBinding,
            });
        }
        if accepted(vfs, directory, member.binding)? {
            decisions.insert(member.binding.transaction, member.binding);
        } else if preparation.is_none()
            && let Some(selected) = selection(vfs, directory)?
            && selected.binding.transaction == member.binding.transaction
        {
            if selected.binding != member.binding {
                return Err(invalid(directory, "root prepared binding mismatch"));
            }
            staged_manifest(vfs, directory, &selected)?;
            decisions.insert(member.binding.transaction, selected.binding);
        }
    }
    Ok(decisions)
}

fn participant_identity(root: &Path, name: &str) -> Result<u128, StoreError> {
    let canonical = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
    Ok(xxhash_rust::xxh3::xxh3_128(
        format!("{}\t{name}", canonical.display()).as_bytes(),
    ))
}

fn rollback_stages(
    ordered: &[&LiveNamespaceMutation<'_>],
    staged: Vec<crate::ingest::NamespaceStage>,
    selections: &[Option<StagedSelection>],
    lengths: &[u64],
    wals: &mut [std::sync::MutexGuard<'_, Option<crate::ingest::StoreWal>>],
) -> Result<(), StoreError> {
    let result = (|| {
        for ((((p, stage), selection), length), wal) in ordered
            .iter()
            .zip(staged)
            .zip(selections)
            .zip(lengths)
            .zip(wals.iter_mut())
        {
            wal.as_mut()
                .ok_or(StoreError::ReadOnly)?
                .abort_namespace_suffix(
                    p.store.vfs.as_ref(),
                    &p.store.directory.join("wal.ze"),
                    *length,
                    p.store.durability_policy,
                    stage.manifest.log_seq,
                    &p.store.accounting,
                )?;
            cleanup_stage(p.store, stage, selection.as_ref())?;
        }
        Ok(())
    })();
    if result.is_err() {
        for wal in wals {
            **wal = None;
        }
    }
    result
}

/// Reclaims unreachable namespace transaction artifacts.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn namespace_reclaim(root: &Path) -> Result<(), StoreError> {
    let root = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
    let _owner = StoreLock::acquire(&root).map_err(StoreError::Lock)?;
    reclamation::run(&StdVfs, &root, &mut |_| Ok(()))
}

#[cfg(test)]
mod portable_tests;
mod reclamation;
pub(super) use reclamation::{
    admission as reader_admission, for_open as reclaim_for_open, lease as reader_lease,
    refuse as refuse_retired,
};

#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn namespace_reclaim_on_vfs(
    root: &Path,
    vfs: &dyn Vfs,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    let _owner = StoreLock::acquire(root).map_err(StoreError::Lock)?;
    reclamation::run(vfs, root, step)
}

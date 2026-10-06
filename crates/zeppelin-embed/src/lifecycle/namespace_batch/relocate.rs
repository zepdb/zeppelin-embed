//! Explicit legacy export. Source admission stays excluded until publication.
use super::*;
use crate::ingest::wal_payload::{self, TransactionBinding};
use crate::manifest::io::DurableLog;

/// Converts a complete legacy namespace root into an absent destination.
///
/// The source is never repaired or rewritten (ownership may create lock stubs).
/// Pending namespace cleanup, busy roots, symlinks and damaged evidence are refused.
/// The destination requires the portable namespace reader. Only whole-root copies
/// are portable. An error during publication is indeterminate: inspect the
/// destination before retrying; it can be absent or a complete converted root.
pub fn namespace_relocate(source_root: &Path, absent_destination: &Path) -> Result<(), StoreError> {
    execute(source_root, absent_destination, &mut |_| Ok(()))
}

/// Interruption seam for conversion copy, rewrite, sync and publication cuts.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn namespace_relocate_with_steps(
    source: &Path,
    destination: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    execute(source, destination, step)
}

struct Inventory {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
    participants: Vec<PathBuf>,
}

// Inventory never follows links, including links in otherwise unused artifacts.
fn inventory(root: &Path) -> Result<Inventory, StoreError> {
    fn visit(root: &Path, relative: &Path, out: &mut Inventory) -> Result<(), StoreError> {
        let directory = root.join(relative);
        let metadata = std::fs::symlink_metadata(&directory).map_err(|e| io(&directory, e))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(
                &directory,
                "conversion requires directories without symlinks",
            ));
        }
        out.directories.push(relative.to_owned());
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|e| io(&directory, e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io(&directory, e))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| io(&path, e))?;
            let child = relative.join(entry.file_name());
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid(&path, "conversion filename UTF-8"))?;
            if kind.is_dir() {
                let depth = child.components().count();
                let transaction = depth == 1 && name.starts_with(".ze-batch-");
                if transaction {
                    if name.contains("..")
                        || !name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
                    {
                        return Err(invalid(&path, "conversion transaction directory"));
                    }
                } else {
                    if !name_valid(&name)
                        || depth > 2
                        || (depth == 2
                            && !relative
                                .to_str()
                                .is_some_and(|s| s.starts_with(".ze-batch-")))
                    {
                        return Err(invalid(&path, "conversion participant location"));
                    }
                    out.participants.push(child.clone());
                }
                visit(root, &child, out)?;
            } else if kind.is_file() {
                if name == ".ze-cleanup" {
                    return Err(invalid(
                        &path,
                        "conversion refuses pending namespace cleanup",
                    ));
                }
                out.files.push(child);
            } else {
                return Err(invalid(
                    &path,
                    "conversion refuses symlinks and special files",
                ));
            }
        }
        Ok(())
    }
    let mut out = Inventory {
        directories: Vec::new(),
        files: Vec::new(),
        participants: Vec::new(),
    };
    visit(root, Path::new(""), &mut out)?;
    out.participants.sort();
    Ok(out)
}

fn descriptor_routes(descriptor: &RootDescriptor) -> &Routes {
    match descriptor {
        RootDescriptor::Legacy(routes) => routes,
        RootDescriptor::Staged(staged) => &staged.1,
    }
}

fn legacy_path(root: &Path, inventory: &Inventory) -> Result<String, StoreError> {
    let mut stored = None;
    for relative in &inventory.files {
        if relative.file_name().and_then(|s| s.to_str()) != Some(REFERENCE) {
            continue;
        }
        let path = root.join(relative);
        let bytes = StdVfs.read(&path).map_err(|e| io(&path, e))?;
        let text = std::str::from_utf8(body(&path, &bytes)?)
            .map_err(|_| invalid(&path, "legacy root UTF-8"))?;
        if !Path::new(text).is_absolute() || stored.as_deref().is_some_and(|prior| prior != text) {
            return Err(invalid(&path, "inconsistent stored legacy root"));
        }
        stored = Some(text.to_owned());
    }
    stored.ok_or_else(|| invalid(root, "conversion requires legacy namespace references"))
}

fn identity(stored: &str, name: &str) -> u128 {
    // Exact stored UTF-8, even when the original directory no longer exists.
    xxhash_rust::xxh3::xxh3_128(format!("{stored}\t{name}").as_bytes())
}

fn binding_file(path: &Path) -> Result<TransactionBinding, StoreError> {
    let bytes = StdVfs.read(path).map_err(|e| io(path, e))?;
    TransactionBinding::decode(body(path, &bytes)?).map_err(|e| invalid(path, &e.to_string()))
}

fn manifest(
    directory: &Path,
    path: &Path,
    end: u64,
) -> Result<crate::manifest::Manifest, StoreError> {
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, path, end).map_err(StoreError::Manifest)?;
    for segment in &manifest.segments {
        crate::segment::reader::validate_header_with_vfs(
            &StdVfs,
            &directory.join(segment.id.file_name()),
            segment,
        )
        .map_err(|e| StoreError::Manifest(crate::manifest::ManifestError::Segment(e)))?;
    }
    Ok(manifest)
}

// Validate using supplied authority, without ordinary open/adoption/reclamation.
fn validate(
    root: &Path,
    inventory: &Inventory,
    descriptor: &RootDescriptor,
    participant_id: &dyn Fn(&str) -> Result<u128, StoreError>,
) -> Result<(), StoreError> {
    let routes = descriptor_routes(descriptor);
    for (name, route) in routes {
        if !inventory.participants.contains(&PathBuf::from(name))
            || !inventory.participants.contains(&PathBuf::from(route))
        {
            return Err(invalid(root, "conversion route participant missing"));
        }
        let intent = root
            .join(route)
            .parent()
            .ok_or_else(|| invalid(root, "route parent"))?
            .join("intent.ze");
        let bytes = StdVfs.read(&intent).map_err(|e| io(&intent, e))?;
        if decode_routes(&intent, &bytes)?.get(name) != Some(route) {
            return Err(invalid(&intent, "conversion route disagrees with intent"));
        }
        let reference = root.join(route).join(REFERENCE);
        if !reference.is_file() {
            return Err(invalid(
                &reference,
                "conversion routed participant reference missing",
            ));
        }
        let prepared = root.join(route).join(PREPARED);
        let bytes = StdVfs.read(&prepared).map_err(|e| io(&prepared, e))?;
        if body(&prepared, &bytes)? != route.as_bytes() {
            return Err(invalid(&prepared, "conversion preparation identity"));
        }
    }
    for relative in &inventory.files {
        let path = root.join(relative);
        let name = relative
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid(&path, "filename"))?;
        if name == "intent.ze" {
            let bytes = StdVfs.read(&path).map_err(|e| io(&path, e))?;
            let intended = decode_routes(&path, &bytes)?;
            for route in intended.values() {
                if Path::new(route).parent() != relative.parent() {
                    return Err(invalid(&path, "foreign preparation intent"));
                }
                if !inventory.participants.contains(&PathBuf::from(route)) {
                    return Err(invalid(&path, "conversion intended participant missing"));
                }
            }
        }
        if matches!(name, PREPARED | ".ze-retired") {
            let bytes = StdVfs.read(&path).map_err(|e| io(&path, e))?;
            if relative.parent().map(|p| p.as_os_str().as_encoded_bytes())
                != Some(body(&path, &bytes)?)
            {
                return Err(invalid(&path, "retained preparation identity"));
            }
            if name == PREPARED {
                let participant = relative
                    .parent()
                    .ok_or_else(|| invalid(&path, "preparation parent"))?;
                let transaction = participant
                    .parent()
                    .ok_or_else(|| invalid(&path, "preparation transaction"))?;
                let intent_path = root.join(transaction).join("intent.ze");
                let intent = StdVfs.read(&intent_path).map_err(|e| io(&intent_path, e))?;
                let name = participant
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| invalid(&path, "preparation name"))?;
                if decode_routes(&intent_path, &intent)?
                    .get(name)
                    .map(Path::new)
                    != Some(participant)
                {
                    return Err(invalid(
                        &intent_path,
                        "retained preparation disagrees with intent",
                    ));
                }
            }
        }
    }
    let mut identities = BTreeMap::new();
    for relative in &inventory.participants {
        let directory = root.join(relative);
        let name = relative
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid(&directory, "participant name"))?;
        let id = participant_id(name)?;
        if id == 0
            || identities
                .insert(id, name)
                .is_some_and(|prior| prior != name)
        {
            return Err(invalid(&directory, "duplicate participant identity"));
        }
        // Fully reclaimed retired directories contain only ownership stubs.
        if !directory.join("manifest.ze").exists()
            && directory.join(".ze-retired").exists()
            && inventory
                .files
                .iter()
                .filter(|p| p.parent() == Some(relative))
                .all(|p| {
                    matches!(
                        p.file_name().and_then(|s| s.to_str()),
                        Some("writer.lock" | ".ze-readers.lock" | REFERENCE | ".ze-retired")
                    )
                })
        {
            continue;
        }
        let wal_path = directory.join("wal.ze");
        let wal = crate::wal::WalReader::open(&StdVfs, &wal_path).map_err(StoreError::Wal)?;
        let end = wal.durable_end();
        let wal = wal.into_clean().map_err(StoreError::WalRecovery)?;
        let current = manifest(&directory, &directory.join("manifest.ze"), end)?;
        let mut decisions = BTreeMap::new();
        for file in inventory
            .files
            .iter()
            .filter(|p| p.parent() == Some(relative))
        {
            let path = root.join(file);
            let filename = file
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid(&path, "filename"))?;
            if filename.starts_with(".ze-manifest-") {
                manifest(&directory, &path, end)?;
            }
            if let Some(transaction) = filename.strip_prefix(".ze-accepted-") {
                let binding = binding_file(&path)?;
                let transaction = transaction.strip_suffix(".tmp").unwrap_or(transaction);
                if transaction != binding.transaction.to_string() || binding.participant != id {
                    return Err(invalid(&path, "conversion acceptance identity"));
                }
                if !filename.ends_with(".tmp") {
                    if current.generation < binding.final_generation {
                        return Err(invalid(&path, "accepted generation missing"));
                    }
                    let retained = format!(".ze-manifest-{}", binding.transaction);
                    if directory.join(&retained).exists() {
                        selected_manifest(
                            &StdVfs,
                            &directory,
                            &StagedSelection {
                                manifest: retained,
                                binding,
                            },
                        )?;
                    }
                    decisions.insert(binding.transaction, binding);
                }
            }
        }
        if let RootDescriptor::Staged(staged) = descriptor
            && let Some(selected) = staged.0.get(name)
            && routes.get(name).map_or(Path::new(name), |r| Path::new(r)) == relative
        {
            if selected.binding.participant != id {
                return Err(invalid(
                    &directory,
                    "conversion staged participant identity",
                ));
            }
            selected_manifest(&StdVfs, &directory, selected)?;
            manifest(&directory, &directory.join(&selected.manifest), end)?;
            if decisions
                .insert(selected.binding.transaction, selected.binding)
                .is_some_and(|prior| prior != selected.binding)
            {
                return Err(invalid(&directory, "conflicting conversion decision"));
            }
            // Selected committed runs always need complete retained framing.
            staged_manifest(&StdVfs, &directory, selected)?;
        }
        for record in wal
            .records()
            .iter()
            .filter(|r| r.op == wal_payload::PREPARED_MUTATION_V1)
        {
            let payload = record
                .payload()
                .map_err(|e| invalid(&wal_path, &e.to_string()))?;
            let member = wal_payload::decode_prepared(payload)
                .map_err(|e| invalid(&wal_path, &e.to_string()))?;
            if member.binding.participant != id || !directory.join(REFERENCE).exists() {
                return Err(invalid(
                    &wal_path,
                    "conversion prepared participant identity",
                ));
            }
        }
        // Validate all retained runs, including absorbed records. Decisions whose
        // complete range was already reclaimed are proved by the canonical manifest.
        crate::ingest::committed_mutations_with_decisions(wal.records(), 0, |binding| {
            decisions.get(&binding.transaction).copied()
        })?;
        for binding in decisions.values() {
            if binding.last_seq > current.log_seq
                || wal
                    .records()
                    .iter()
                    .any(|r| (binding.first_seq..=binding.last_seq).contains(&r.seq.get()))
            {
                let records = wal
                    .records()
                    .iter()
                    .filter(|r| (binding.first_seq..=binding.last_seq).contains(&r.seq.get()))
                    .count();
                if records as u64 != binding.last_seq - binding.first_seq + 1 {
                    return Err(invalid(&directory, "committed prepared range incomplete"));
                }
                // committed_batches above validates exact binding, member position and sequence.
            }
        }
    }
    if let RootDescriptor::Staged(staged) = descriptor {
        for name in staged.0.keys() {
            let selected = routes.get(name).map_or(name.as_str(), String::as_str);
            if !inventory.participants.contains(&PathBuf::from(selected)) {
                return Err(invalid(root, "staged participant missing"));
            }
        }
    }
    Ok(())
}

fn rewrite_binding(
    mut binding: TransactionBinding,
    stored: &str,
    id: NamespaceRootId,
    name: &str,
    path: &Path,
) -> Result<TransactionBinding, StoreError> {
    if binding.participant != identity(stored, name) {
        return Err(invalid(path, "conversion legacy binding mismatch"));
    }
    binding.participant = portable::participant_id(id, name)?;
    Ok(binding)
}

fn rewrite_descriptor(
    mut descriptor: RootDescriptor,
    stored: &str,
    id: NamespaceRootId,
    path: &Path,
) -> Result<Vec<u8>, StoreError> {
    if let RootDescriptor::Staged(staged) = &mut descriptor {
        for (name, selected) in &mut staged.0 {
            selected.binding = rewrite_binding(selected.binding, stored, id, name, path)?;
        }
    }
    portable::encode_root(id, &descriptor)
}

fn rewrite(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    stored: &str,
    id: NamespaceRootId,
) -> Result<Option<Vec<u8>>, StoreError> {
    let path = root.join(relative);
    let filename = relative
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid(&path, "filename"))?;
    if matches!(filename, RECORD | ".ze-namespaces.tmp") {
        return rewrite_descriptor(decode_descriptor(&path, bytes)?, stored, id, &path).map(Some);
    }
    let participant = relative
        .parent()
        .ok_or_else(|| invalid(&path, "participant parent"))?;
    let name = participant
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if matches!(filename, REFERENCE | ".ze-namespace-root.tmp") {
        if body(&path, bytes)? != stored.as_bytes() {
            return Err(invalid(&path, "conversion reference disagreement"));
        }
        return encode_participant_reference(
            Some(id),
            root,
            name,
            participant.components().count() as u8,
        )
        .map(Some);
    }
    if filename.starts_with(".ze-accepted-") {
        let binding = TransactionBinding::decode(body(&path, bytes)?)
            .map_err(|e| invalid(&path, &e.to_string()))?;
        return Ok(Some(envelope(
            &rewrite_binding(binding, stored, id, name, &path)?
                .encode()
                .map_err(|e| invalid(&path, &e.to_string()))?,
        )));
    }
    if filename != "wal.ze" {
        return Ok(None);
    }
    let wal = crate::wal::WalReader::open(&StdVfs, &path)
        .map_err(StoreError::Wal)?
        .into_clean()
        .map_err(StoreError::WalRecovery)?;
    let mut rewritten = bytes
        .get(..crate::wal::header::WAL_HEADER_LEN)
        .ok_or_else(|| invalid(&path, "WAL header"))?
        .to_vec();
    for record in wal.records() {
        if record.op != wal_payload::PREPARED_MUTATION_V1 {
            rewritten.extend_from_slice(
                record
                    .encoded()
                    .map_err(|e| invalid(&path, &e.to_string()))?,
            );
            continue;
        }
        let payload = record
            .payload()
            .map_err(|e| invalid(&path, &e.to_string()))?;
        let member =
            wal_payload::decode_prepared(payload).map_err(|e| invalid(&path, &e.to_string()))?;
        let binding = rewrite_binding(member.binding, stored, id, name, &path)?;
        // Only the frozen binding changes. Preserve version/flags, index/count,
        // inner op and the original inner body rather than re-encoding mutations.
        let mut body = payload
            .get(..4)
            .ok_or_else(|| invalid(&path, "prepared header"))?
            .to_vec();
        body.extend_from_slice(
            &binding
                .encode()
                .map_err(|e| invalid(&path, &e.to_string()))?,
        );
        body.extend_from_slice(
            payload
                .get(68..)
                .ok_or_else(|| invalid(&path, "prepared member"))?,
        );
        rewritten.extend_from_slice(
            &crate::wal::record::encode_record(crate::wal::record::WalRecord {
                seq: record.seq,
                op: record.op,
                payload: &body,
            })
            .map_err(|e| invalid(&path, &e.to_string()))?,
        );
    }
    Ok(Some(rewritten))
}

fn absent(path: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
        Ok(_) => Err(io(
            path,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "conversion destination must be absent",
            ),
        )),
    }
}

// One exclusive rename; never replace even an empty competing destination.
fn publish_absent(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        // SAFETY: both C strings remain live; RENAME_EXCL forbids replacement.
        let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        // SAFETY: both C strings remain live; the flag forbids replacement.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exclusive conversion publication unsupported",
        ));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        std::fs::rename(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (from, to);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exclusive conversion publication unsupported",
        ))
    }
}

fn execute(
    source: &Path,
    destination: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    inventory(source)?;
    let source = std::fs::canonicalize(source).map_err(|e| io(source, e))?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid(destination, "destination final name"))?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent).map_err(|e| io(parent, e))?;
    let destination = parent.join(name);
    absent(&destination)?;
    if destination.starts_with(&source) {
        return Err(invalid(
            &destination,
            "conversion destination inside source",
        ));
    }
    let _coordinator = StoreLock::acquire(&source).map_err(StoreError::Lock)?;
    let _admission = StoreLock::reclaim_exclusive(&source).map_err(StoreError::Lock)?;
    let inventory = inventory(&source)?;
    let _participants = inventory
        .participants
        .iter()
        .map(|p| StoreLock::acquire(&source.join(p)).map_err(StoreError::Lock))
        .collect::<Result<Vec<_>, _>>()?;
    let path = source.join(RECORD);
    let bytes = StdVfs.read(&path).map_err(|e| io(&path, e))?;
    let descriptor = decode_descriptor(&path, &bytes)?;
    let stored = legacy_path(&source, &inventory)?;
    validate(&source, &inventory, &descriptor, &|name| {
        Ok(identity(&stored, name))
    })?;
    let id = NamespaceRootId::generate().map_err(|e| io(&source, e))?;
    let staging = parent.join(format!(".ze-relocate-{:032x}.tmp", id.get()));
    for relative in &inventory.directories {
        let path = staging.join(relative);
        std::fs::create_dir(&path).map_err(|e| io(&path, e))?;
    }
    for relative in &inventory.files {
        let from = source.join(relative);
        let to = staging.join(relative);
        std::fs::copy(&from, &to).map_err(|e| io(&to, e))?;
        step("conversion copy").map_err(|e| io(&to, e))?;
        let filename = relative
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid(&from, "filename"))?;
        if matches!(
            filename,
            RECORD | ".ze-namespaces.tmp" | REFERENCE | ".ze-namespace-root.tmp" | "wal.ze"
        ) || filename.starts_with(".ze-accepted-")
        {
            let bytes = StdVfs.read(&from).map_err(|e| io(&from, e))?;
            if let Some(bytes) = rewrite(&source, relative, &bytes, &stored, id)? {
                StdVfs.write(&to, &bytes).map_err(|e| io(&to, e))?;
                step("conversion write").map_err(|e| io(&to, e))?;
            }
        }
    }
    let (staged_id, staged_descriptor) = root_record_vfs(&StdVfs, &staging)?;
    if staged_id != Some(id) {
        return Err(invalid(&staging, "converted root identity"));
    }
    for relative in &inventory.participants {
        let directory = staging.join(relative);
        if directory.join(REFERENCE).exists() {
            let name = relative
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid(&directory, "participant name"))?;
            let selected = descriptor_routes(&staged_descriptor)
                .get(name)
                .is_some_and(|r| Path::new(r) == relative);
            let preparation = (relative.components().count() == 2 && !selected)
                .then(|| PrivatePreparation::new(id, name, &directory, &_coordinator));
            let authority =
                portable::authority_with_preparation(&StdVfs, &directory, preparation.as_ref())?;
            if authority != Some((staging.as_path(), name, id)) {
                return Err(invalid(&directory, "converted participant authority"));
            }
        }
    }
    validate(&staging, &inventory, &staged_descriptor, &|name| {
        portable::participant_id(id, name)
    })?;
    for relative in &inventory.files {
        let path = staging.join(relative);
        StdVfs
            .sync(&path, SyncKind::Full)
            .map_err(|e| io(&path, e))?;
        step("conversion file sync").map_err(|e| io(&path, e))?;
    }
    for relative in inventory.directories.iter().rev() {
        let path = staging.join(relative);
        StdVfs
            .sync(&path, SyncKind::Full)
            .map_err(|e| io(&path, e))?;
        step("conversion directory sync").map_err(|e| io(&path, e))?;
    }
    step("conversion before publish").map_err(|e| io(&staging, e))?;
    let mut publication = || -> std::io::Result<()> {
        publish_absent(&staging, &destination)?;
        step("conversion publish")?;
        StdVfs.sync(&parent, SyncKind::Full)?;
        step("conversion parent sync")
    };
    publication().map_err(|e| {
        io(
            &destination,
            std::io::Error::new(
                e.kind(),
                format!("namespace relocation publication is indeterminate: {e}"),
            ),
        )
    })
}

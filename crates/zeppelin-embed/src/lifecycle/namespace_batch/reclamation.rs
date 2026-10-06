//! Root-owned mark/unlink protocol. Lock stubs are never unlinked: replacing a
//! lock inode would let a newly admitted reader evade an existing OS lock.
use super::*;
use std::collections::BTreeSet;
const INTENT: &str = ".ze-cleanup";
const RETIRED: &str = ".ze-retired";

fn try_lock(
    result: Result<StoreLock, super::super::lock::StoreLockError>,
) -> Result<Option<StoreLock>, StoreError> {
    match result {
        Ok(lock) => Ok(Some(lock)),
        Err(super::super::lock::StoreLockError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Ok(None)
        }
        Err(error) => Err(StoreError::Lock(error)),
    }
}

pub(in crate::lifecycle) fn lease(
    path: &Path,
    writable: bool,
) -> Result<Option<StoreLock>, StoreError> {
    let result = if writable {
        StoreLock::reader_lease(path).map(Some)
    } else {
        StoreLock::reader_lease_read_only(path)
    };
    result.map_err(|error| match error {
        super::super::lock::StoreLockError::Io { ref source, .. }
            if source.kind() == std::io::ErrorKind::WouldBlock =>
        {
            StoreError::StoreBusy {
                path: path.to_path_buf(),
            }
        }
        error => StoreError::Lock(error),
    })
}

pub(in crate::lifecycle) fn admission(
    path: &Path,
    writable: bool,
) -> Result<Option<StoreLock>, StoreError> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let root = if let Some((root, _, _)) = portable::authority(&StdVfs, path)? {
        root
    } else if parent
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with(".ze-batch-"))
    {
        parent
            .parent()
            .ok_or_else(|| invalid(path, "reader route root"))?
    } else {
        parent
    };
    if read_optional(&root.join(RECORD))?.is_some() {
        lease(root, writable)
    } else {
        Ok(None)
    }
}

pub(in crate::lifecycle) fn for_open(path: &Path) -> Result<(), StoreError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let root = if let Some((root, _, _)) = portable::authority(&StdVfs, path)? {
        root
    } else if parent
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with(".ze-batch-"))
    {
        parent
            .parent()
            .ok_or_else(|| invalid(path, "cleanup route root"))?
    } else {
        parent
    };
    if read_optional(&root.join(RECORD))?.is_none() {
        return Ok(());
    }
    let Some(_owner) = try_lock(StoreLock::acquire(root))? else {
        return Ok(());
    };
    run(&StdVfs, root, &mut |_| Ok(()))
}

pub(in crate::lifecycle) fn refuse(vfs: &dyn Vfs, directory: &Path) -> Result<(), StoreError> {
    if let Some(bytes) = read_optional_vfs(vfs, &directory.join(RETIRED))? {
        body(&directory.join(RETIRED), &bytes)?;
        return Err(invalid(directory, "retired namespace storage"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Garbage {
    directory: String,
    identity: (u64, u64),
    retired: bool,
    files: BTreeMap<String, u64>,
}

fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}
fn encode_intent(items: &[Garbage]) -> Vec<u8> {
    let mut text = String::from("ZECLEAN1\n");
    for item in items {
        for (file, digest) in &item.files {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                item.directory,
                item.identity.0,
                item.identity.1,
                u8::from(item.retired),
                file,
                digest
            ));
        }
    }
    envelope(text.as_bytes())
}
fn decode_intent(path: &Path, bytes: &[u8]) -> Result<Vec<Garbage>, StoreError> {
    let text =
        std::str::from_utf8(body(path, bytes)?).map_err(|_| invalid(path, "cleanup UTF-8"))?;
    let text = text
        .strip_prefix("ZECLEAN1\n")
        .ok_or_else(|| invalid(path, "cleanup version"))?;
    let mut items: BTreeMap<String, Garbage> = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let directory = fields
            .next()
            .ok_or_else(|| invalid(path, "cleanup directory"))?;
        let components = directory.split('/').collect::<Vec<_>>();
        if !(components.len() == 1 || components.len() == 2)
            || !(directory == "." || components.iter().all(|c| safe_component(c)))
            || (components.len() == 2 && !directory.starts_with(".ze-batch-"))
        {
            return Err(invalid(path, "cleanup directory grammar"));
        }
        let mut number = || {
            fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| invalid(path, "cleanup number"))
        };
        let identity = (number()?, number()?);
        let retired = match number()? {
            0 => false,
            1 => true,
            _ => return Err(invalid(path, "cleanup retirement")),
        };
        let file = fields
            .next()
            .filter(|s| safe_component(s))
            .ok_or_else(|| invalid(path, "cleanup file"))?;
        let digest = fields
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| invalid(path, "cleanup digest"))?;
        if fields.next().is_some() {
            return Err(invalid(path, "cleanup fields"));
        }
        let item = items.entry(directory.into()).or_insert_with(|| Garbage {
            directory: directory.into(),
            identity,
            retired,
            files: BTreeMap::new(),
        });
        if item.identity != identity
            || item.retired != retired
            || item.files.insert(file.into(), digest).is_some()
        {
            return Err(invalid(path, "cleanup identity or duplicate"));
        }
    }
    let result = items.into_values().collect::<Vec<_>>();
    if encode_intent(&result) != bytes {
        return Err(invalid(path, "noncanonical cleanup"));
    }
    Ok(result)
}

fn routes_and_marks(
    vfs: &dyn Vfs,
    root: &Path,
) -> Result<(Routes, BTreeMap<String, String>), StoreError> {
    let (root_id, descriptor) = root_record_vfs(vfs, root)?;
    let (routes, pending) = match descriptor {
        RootDescriptor::Legacy(routes) => (routes, BTreeMap::new()),
        RootDescriptor::Staged(descriptor) => {
            let mut pending = BTreeMap::new();
            for (name, selected) in descriptor.0 {
                let directory = descriptor
                    .1
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                let identity = if root_id.is_some() {
                    reader_participant_identity(vfs, &root.join(&directory))?
                } else {
                    participant_identity(root, &name)?
                };
                if selected.binding.participant != identity {
                    return Err(invalid(root, "pending cleanup participant identity"));
                }
                if accepted(vfs, &root.join(&directory), selected.binding)? {
                    selected_manifest(vfs, &root.join(&directory), &selected)?;
                } else {
                    staged_manifest(vfs, &root.join(&directory), &selected)?;
                }
                pending.insert(directory, selected.manifest);
            }
            (descriptor.1, pending)
        }
    };
    for (name, route) in &routes {
        let directory = root.join(route);
        let parent = directory
            .parent()
            .ok_or_else(|| invalid(&directory, "route parent"))?;
        let intent_path = parent.join("intent.ze");
        let bytes = vfs.read(&intent_path).map_err(|e| io(&intent_path, e))?;
        if decode_routes(&intent_path, &bytes)?.get(name) != Some(route) {
            return Err(invalid(&intent_path, "route disagrees with intent"));
        }
        let prepared = directory.join(PREPARED);
        let bytes = vfs.read(&prepared).map_err(|e| io(&prepared, e))?;
        if body(&prepared, &bytes)? != route.as_bytes() {
            return Err(invalid(&prepared, "route preparation identity"));
        }
        for file in ["manifest.ze", "wal.ze"] {
            let path = directory.join(file);
            if vfs.open(&path).map_err(|e| io(&path, e))? == 0 {
                return Err(invalid(&path, "empty routed artifact"));
            }
        }
    }
    Ok((routes, pending))
}

fn read_payload_optional(vfs: &dyn Vfs, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid(path, "cleanup candidate is not a regular file"));
        }
        Ok(_) => {}
        // Virtual VFS crash images may have no corresponding OS file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(path, error)),
    }
    match vfs.read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

fn payload(name: &str) -> bool {
    name == "intent.ze"
        || name == ".ze-namespaces.tmp"
        || name == "manifest.ze"
        || name == "wal.ze"
        || name == ".manifest.ze.tmp"
        || name == ".ze-prepared"
        || name == ".ze-namespace-root.tmp"
        || (name.starts_with("segment-") && name.ends_with(".zseg"))
        || (name.starts_with(".segment-") && name.ends_with(".zseg.tmp"))
        || name.starts_with(".ze-manifest-")
        || name.starts_with(".ze-accepted-")
}

fn garbage(
    vfs: &dyn Vfs,
    root: &Path,
    relative: &str,
    routes: &Routes,
    pending: &BTreeMap<String, String>,
) -> Result<Garbage, StoreError> {
    let directory = root.join(relative);
    let metadata = std::fs::symlink_metadata(&directory).map_err(|e| io(&directory, e))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(&directory, "cleanup directory identity"));
    }
    let identity = super::super::lock::store_identity(&directory).map_err(|e| io(&directory, e))?;
    if let Some(bytes) = read_optional_vfs(vfs, &directory.join(RETIRED))?
        && body(&directory.join(RETIRED), &bytes)? != relative.as_bytes()
    {
        return Err(invalid(&directory, "retired storage identity"));
    }
    let transaction = relative.starts_with(".ze-batch-") && !relative.contains('/');
    if relative == "." {
        let mut files = BTreeMap::new();
        for name in [".ze-namespaces.tmp", ".ze-cleanup.tmp"] {
            let path = root.join(name);
            if let Some(bytes) = read_payload_optional(vfs, &path)? {
                files.insert(name.into(), xxh3_64(&bytes));
            }
        }
        return Ok(Garbage {
            directory: relative.into(),
            identity,
            retired: false,
            files,
        });
    }
    if transaction {
        let retired = !routes
            .values()
            .any(|route| route.starts_with(&format!("{relative}/")));
        let mut files = BTreeMap::new();
        let path = directory.join("intent.ze");
        if let Some(bytes) = read_payload_optional(vfs, &path)? {
            decode_routes(&path, &bytes)?;
            if retired {
                files.insert("intent.ze".into(), xxh3_64(&bytes));
            }
        }
        return Ok(Garbage {
            directory: relative.into(),
            identity,
            retired,
            files,
        });
    }
    let retired = if relative.contains('/') {
        !routes.values().any(|route| route == relative)
    } else {
        routes.contains_key(relative)
    };
    let mut marked = BTreeSet::new();
    if !retired {
        for name in [
            Some("manifest.ze"),
            pending.get(relative).map(String::as_str),
        ]
        .into_iter()
        .flatten()
        {
            let path = directory.join(name);
            if let Some(bytes) = read_payload_optional(vfs, &path)? {
                let manifest =
                    crate::manifest::decode_manifest(&path.display().to_string(), &bytes)
                        .map_err(StoreError::Manifest)?;
                for segment in manifest.segments {
                    marked.insert(segment.id.file_name());
                }
                marked.insert(name.to_owned());
            }
        }
    }
    let mut files = BTreeMap::new();
    for path in vfs.list(&directory).map_err(|e| io(&directory, e))? {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid(&path, "cleanup filename"))?;
        if !payload(name) {
            continue;
        }
        if !safe_component(name) {
            return Err(invalid(&path, "cleanup filename grammar"));
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid(&path, "cleanup candidate is not a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&path, error)),
        }
        let staged = name.starts_with(".ze-manifest-");
        if name == PREPARED {
            let bytes = vfs.read(&path).map_err(|e| io(&path, e))?;
            if body(&path, &bytes)? != relative.as_bytes() {
                return Err(invalid(&path, "cleanup preparation identity"));
            }
        }
        if staged {
            let bytes = vfs.read(&path).map_err(|e| io(&path, e))?;
            crate::manifest::decode_manifest(&path.display().to_string(), &bytes)
                .map_err(StoreError::Manifest)?;
        }
        if payload(name)
            && !marked.contains(name)
            && (retired
                || staged
                || name == ".manifest.ze.tmp"
                || name == ".ze-namespace-root.tmp"
                || (name.starts_with(".ze-accepted-") && name.ends_with(".tmp"))
                || (name.starts_with(".segment-") && name.ends_with(".zseg.tmp"))
                || (name.starts_with("segment-") && name.ends_with(".zseg")))
        {
            files.insert(
                name.into(),
                xxh3_64(&vfs.read(&path).map_err(|e| io(&path, e))?),
            );
        }
    }
    Ok(Garbage {
        directory: relative.into(),
        identity,
        retired,
        files,
    })
}

fn directories(root: &Path) -> Result<Vec<String>, StoreError> {
    let mut result = vec![".".to_owned()];
    for entry in std::fs::read_dir(root).map_err(|e| io(root, e))? {
        let entry = entry.map_err(|e| io(root, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid(root, "cleanup directory UTF-8"))?;
        if name.starts_with(".ze-batch-") {
            if !safe_component(&name) || !entry.file_type().map_err(|e| io(root, e))?.is_dir() {
                return Err(invalid(&entry.path(), "transaction directory"));
            }
            result.push(name.clone());
            for child in std::fs::read_dir(entry.path()).map_err(|e| io(&entry.path(), e))? {
                let child = child.map_err(|e| io(&entry.path(), e))?;
                let child_name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid(root, "cleanup child UTF-8"))?;
                if child
                    .file_type()
                    .map_err(|e| io(&child.path(), e))?
                    .is_dir()
                {
                    if !name_valid(&child_name) {
                        return Err(invalid(&child.path(), "transaction namespace"));
                    }
                    result.push(format!("{name}/{child_name}"));
                }
            }
        } else if name_valid(&name) && entry.file_type().map_err(|e| io(root, e))?.is_dir() {
            result.push(name);
        }
    }
    result.sort();
    Ok(result)
}

// A deleting batch cannot acknowledge erasure while an older engine-owned
// copy remains pinned. The ordinary collector may skip it; deleting admission
// must instead refuse before publication.
pub(super) fn require_retired_erased(vfs: &dyn Vfs, root: &Path) -> Result<(), StoreError> {
    let (routes, pending) = routes_and_marks(vfs, root)?;
    for relative in directories(root)? {
        let item = garbage(vfs, root, &relative, &routes, &pending)?;
        if item.retired && !item.files.is_empty() {
            return Err(StoreError::StoreBusy {
                path: root.join(relative),
            });
        }
    }
    Ok(())
}

pub(super) fn run(
    vfs: &dyn Vfs,
    root: &Path,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    // Excludes admission from resolution until the physical directory is pinned.
    let Some(_admission) = try_lock(StoreLock::reclaim_exclusive(root))? else {
        return if read_optional_vfs(vfs, &root.join(INTENT))?.is_some() {
            Err(StoreError::StoreBusy {
                path: root.to_path_buf(),
            })
        } else {
            Ok(())
        };
    };
    let intent = root.join(INTENT);
    let (routes, pending) = routes_and_marks(vfs, root)?;
    let mut locks = Vec::new();
    let items = if let Some(bytes) = read_optional_vfs(vfs, &intent)? {
        let items = decode_intent(&intent, &bytes)?;
        // A surviving rename alone does not establish power-cut durability.
        vfs.sync(&intent, SyncKind::Full)
            .map_err(|e| io(&intent, e))?;
        sync_dir(vfs, root, step)?;
        items
    } else {
        let mut items = Vec::new();
        for relative in directories(root)? {
            let directory = root.join(&relative);
            if relative == "." {
                let item = garbage(vfs, root, &relative, &routes, &pending)?;
                if !item.files.is_empty() {
                    items.push(item);
                }
                continue;
            }
            match super::super::refuse_native_graph_directory(vfs, &directory) {
                Err(StoreError::NativeGraphDirectory { .. }) => continue,
                Err(error) => return Err(error),
                Ok(()) => {}
            }
            // Retaining any child retains its transaction metadata too.
            let mut child_locks = Vec::new();
            let mut busy = false;
            if relative.starts_with(".ze-batch-") && !relative.contains('/') {
                for child in std::fs::read_dir(&directory).map_err(|e| io(&directory, e))? {
                    let child = child.map_err(|e| io(&directory, e))?;
                    if child
                        .file_type()
                        .map_err(|e| io(&child.path(), e))?
                        .is_dir()
                    {
                        match try_lock(StoreLock::reclaim_exclusive(&child.path()))? {
                            Some(lock) => child_locks.push(lock),
                            None => {
                                busy = true;
                                break;
                            }
                        }
                    }
                }
            }
            if busy {
                continue;
            }
            let Some(pin) = try_lock(StoreLock::reclaim_exclusive(&directory))? else {
                continue;
            };
            let Some(writer) = try_lock(StoreLock::acquire(&directory))? else {
                continue;
            };
            let item = garbage(vfs, root, &relative, &routes, &pending)?;
            if !item.files.is_empty() {
                locks.push((pin, writer));
                items.push(item);
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        let temporary = root.join(".ze-cleanup.tmp");
        let bytes = encode_intent(&items);
        if bytes.len() > MAX_RECORD {
            return Err(invalid(&intent, "cleanup intent too large"));
        }
        durable_write(vfs, &temporary, &bytes, step)?;
        vfs.rename(&temporary, &intent)
            .map_err(|e| io(&intent, e))?;
        step("cleanup intent rename").map_err(|e| io(&intent, e))?;
        sync_dir(vfs, root, step)?;
        items
    };
    // On resume locks must be reacquired. Keep every candidate locked until the
    // completion record is durable; never unlink a reader's retained paths.
    if locks.is_empty() {
        for item in &items {
            let directory = root.join(&item.directory);
            if item.directory == "." {
                continue;
            }
            let Some(pin) = try_lock(StoreLock::reclaim_exclusive(&directory))? else {
                return Err(StoreError::StoreBusy { path: directory });
            };
            let Some(writer) = try_lock(StoreLock::acquire(&directory))? else {
                return Err(StoreError::StoreBusy { path: directory });
            };
            locks.push((pin, writer));
        }
    }
    let (routes, pending) = routes_and_marks(vfs, root)?;
    for item in &items {
        let directory = root.join(&item.directory);
        let current = garbage(vfs, root, &item.directory, &routes, &pending)?;
        if current.identity != item.identity || current.retired != item.retired {
            return Err(invalid(&directory, "cleanup identity/reachability changed"));
        }
        if item.retired {
            let marker = directory.join(RETIRED);
            if read_optional_vfs(vfs, &marker)?.is_none() {
                durable_write(vfs, &marker, &envelope(item.directory.as_bytes()), step)?;
            } else {
                vfs.sync(&marker, SyncKind::Full)
                    .map_err(|e| io(&marker, e))?;
            }
            sync_dir(vfs, &directory, step)?;
        }
        for (name, digest) in &item.files {
            let path = directory.join(name);
            if let Some(bytes) = read_payload_optional(vfs, &path)? {
                if current.files.get(name) != Some(digest) || xxh3_64(&bytes) != *digest {
                    return Err(invalid(&path, "cleanup file identity/reachability changed"));
                }
                vfs.delete(&path).map_err(|e| io(&path, e))?;
                step("cleanup unlink").map_err(|e| io(&path, e))?;
            }
        }
        sync_dir(vfs, &directory, step)?;
    }
    vfs.delete(&intent).map_err(|e| io(&intent, e))?;
    step("cleanup completion").map_err(|e| io(&intent, e))?;
    sync_dir(vfs, root, step)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ze270_garbage_refuses_symlink_candidate() {
        let root = tempfile::tempdir().expect("root");
        let directory = root.path().join("a");
        std::fs::create_dir(&directory).expect("directory");
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"outside payload").expect("outside");
        std::os::unix::fs::symlink(&outside, directory.join("segment-planted.zseg"))
            .expect("symlink");
        assert!(garbage(&StdVfs, root.path(), "a", &Routes::new(), &BTreeMap::new()).is_err());
    }

    #[test]
    fn namespace_cleanup_intent_format_and_rejection() {
        let items = vec![Garbage {
            directory: "a".into(),
            identity: (11, 22),
            retired: false,
            files: BTreeMap::from([(".ze-manifest-123".into(), 99)]),
        }];
        let mut golden = b"ZENS0001ZECLEAN1\na\t11\t22\t0\t.ze-manifest-123\t99\n".to_vec();
        golden.extend_from_slice(&0x4722_3bce_642b_b7a7_u64.to_le_bytes());
        assert_eq!(encode_intent(&items), golden);
        assert_eq!(
            decode_intent(Path::new("intent"), &golden).expect("golden"),
            items
        );
        for text in [
            "ZECLEAN2\na\t11\t22\t0\t.ze-manifest-123\t99\n",
            "ZECLEAN1\n../a\t11\t22\t0\twal.ze\t99\n",
            "ZECLEAN1\na\t11\t22\t2\twal.ze\t99\n",
            "ZECLEAN1\na\t11\t22\t0\twal.ze\t99\na\t11\t22\t0\twal.ze\t99\n",
            "ZECLEAN1\na\t11\t22\t0\twal.ze\t99\nb\t11\t22\t0\t../wal.ze\t99\n",
        ] {
            assert!(decode_intent(Path::new("intent"), &envelope(text.as_bytes())).is_err());
        }
        golden.pop();
        assert!(decode_intent(Path::new("intent"), &golden).is_err());
    }
}

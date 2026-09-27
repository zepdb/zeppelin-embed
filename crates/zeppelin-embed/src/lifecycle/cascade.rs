//! Root-scoped cascade declarations, serialized with the namespace coordinator.
use super::namespace_batch::{
    body, durable_write, envelope, invalid, io, name_valid, read_optional, sync_dir,
};
use super::{
    CancelToken, DocumentFields, DocumentScanRequest, NamespaceMutation, QueryControl, Store,
    StoreError,
};
use crate::ingest::DocId;
use crate::meta::{ColumnId, ColumnType, PredicateValue};
use crate::vfs::{StdVfs, Vfs};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

const RECORD: &str = ".ze-cascades";
const VERSION: &str = "ZECASCADE1\n";

/// A child namespace's ownership reference to a parent namespace.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CascadeRule {
    /// Parent namespace in the same root.
    pub parent: String,
    /// Child namespace in the same root.
    pub child: String,
    /// Child's id128 attribute holding the parent document ID.
    pub attribute: ColumnId,
}

fn encode(rules: &BTreeSet<CascadeRule>) -> Vec<u8> {
    let mut text = VERSION.to_owned();
    for rule in rules {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            rule.parent,
            rule.child,
            rule.attribute.get()
        ));
    }
    envelope(text.as_bytes())
}
fn read(root: &Path) -> Result<BTreeSet<CascadeRule>, StoreError> {
    let path = root.join(RECORD);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(BTreeSet::new());
    };
    let text =
        std::str::from_utf8(body(&path, &bytes)?).map_err(|_| invalid(&path, "cascade UTF-8"))?;
    let text = text
        .strip_prefix(VERSION)
        .ok_or_else(|| invalid(&path, "cascade version"))?;
    let mut rules = BTreeSet::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let parent = fields
            .next()
            .ok_or_else(|| invalid(&path, "cascade parent"))?;
        let child = fields
            .next()
            .ok_or_else(|| invalid(&path, "cascade child"))?;
        let attribute = fields
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| invalid(&path, "cascade attribute"))?;
        if fields.next().is_some() || !name_valid(parent) || !name_valid(child) {
            return Err(invalid(&path, "invalid cascade rule"));
        }
        let rule = CascadeRule {
            parent: parent.into(),
            child: child.into(),
            attribute: ColumnId::new(attribute),
        };
        reject_cycle(&rules, &rule)?;
        if !rules.insert(rule) {
            return Err(invalid(&path, "duplicate cascade rule"));
        }
    }
    if encode(&rules) != bytes {
        return Err(invalid(&path, "noncanonical cascade record"));
    }
    Ok(rules)
}
fn reject_cycle(rules: &BTreeSet<CascadeRule>, new: &CascadeRule) -> Result<(), StoreError> {
    let mut queue = VecDeque::from([(
        new.child.clone(),
        vec![new.parent.clone(), new.child.clone()],
    )]);
    let mut visited = BTreeSet::new();
    while let Some((name, path)) = queue.pop_front() {
        if name == new.parent {
            return Err(StoreError::CascadeCycle { cycle: path });
        }
        if !visited.insert(name.clone()) {
            continue;
        }
        for rule in rules.iter().filter(|r| r.parent == name) {
            let mut path = path.clone();
            path.push(rule.child.clone());
            queue.push_back((rule.child.clone(), path));
        }
    }
    Ok(())
}
fn validate(
    root: &Path,
    sources: &BTreeMap<String, Store>,
    rule: &CascadeRule,
) -> Result<(), StoreError> {
    if !sources.contains_key(&rule.parent) {
        return Err(invalid(
            root,
            &format!("missing cascade participant {}", rule.parent),
        ));
    }
    let child = sources
        .get(&rule.child)
        .ok_or_else(|| invalid(root, &format!("missing cascade participant {}", rule.child)))?;
    if child
        .schema()
        .column(rule.attribute)
        .map(|c| c.column_type())
        != Some(ColumnType::Id128)
    {
        return Err(invalid(
            root,
            "cascade attribute must be a declared id128 column",
        ));
    }
    Ok(())
}

pub(super) fn declare(
    root: &Path,
    sources: &BTreeMap<String, Store>,
    rule: CascadeRule,
    step: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    validate(root, sources, &rule)?;
    let mut rules = read(root)?;
    reject_cycle(&rules, &rule)?;
    if !rules.insert(rule) {
        return Ok(());
    }
    let bytes = encode(&rules);
    if bytes.len() > 1024 * 1024 {
        return Err(invalid(root, "cascade record too large"));
    }
    let temporary = root.join(".ze-cascades.tmp");
    durable_write(&StdVfs, &temporary, &bytes, step)?;
    StdVfs
        .rename(&temporary, &root.join(RECORD))
        .map_err(|e| io(root, e))?;
    step("cascade declaration rename").map_err(|e| io(root, e))?;
    sync_dir(&StdVfs, root, step)
}

pub(super) fn expand(
    root: &Path,
    sources: &BTreeMap<String, Store>,
    mutations: &mut [NamespaceMutation],
) -> Result<(), StoreError> {
    let rules = read(root)?;
    // Require the entire declared closure even when a parent currently has no
    // matching rows. Missing namespaces must never silently disable a rule.
    let mut reachable: BTreeSet<String> = sources.keys().cloned().collect();
    loop {
        let before = reachable.len();
        for rule in &rules {
            if reachable.contains(&rule.parent) {
                reachable.insert(rule.child.clone());
            }
        }
        if before == reachable.len() {
            break;
        }
    }
    for name in &reachable {
        if !sources.contains_key(name) {
            return Err(invalid(
                root,
                &format!("missing cascade participant {name}"),
            ));
        }
    }
    let active: Vec<_> = rules
        .iter()
        .filter(|r| reachable.contains(&r.parent))
        .collect();
    for rule in &active {
        validate(root, sources, rule)?;
    }
    let mut ids: BTreeMap<String, BTreeSet<DocId>> = mutations
        .iter()
        .map(|m| (m.name.clone(), m.deletes.iter().copied().collect()))
        .collect();
    // Read references once while all logical writer locks are held. Null
    // attributes have no parent; shared children are deleted if any owner is.
    let mut references = Vec::new();
    for (name, source) in sources {
        let child_rules: Vec<_> = active.iter().filter(|r| r.child == *name).collect();
        if child_rules.is_empty() {
            continue;
        }
        let mut cursor = None;
        loop {
            let mut request = DocumentScanRequest::new(
                1024,
                DocumentFields::ATTRIBUTES,
                QueryControl::Cancel(CancelToken::new()),
            );
            if let Some(cursor) = cursor {
                request = request.with_cursor(cursor);
            }
            let page = source
                .scan_documents(request)
                .map_err(|e| invalid(root, &e.to_string()))?;
            for document in page.documents {
                for (column, value) in document.attributes.into_iter().flatten() {
                    for rule in child_rules.iter().filter(|r| r.attribute == column) {
                        let PredicateValue::Id128(parent_id) = value else {
                            return Err(invalid(root, "cascade reference is not id128"));
                        };
                        references.push((
                            rule.parent.clone(),
                            parent_id,
                            name.clone(),
                            document.doc_id,
                        ));
                    }
                }
            }
            cursor = page.continuation;
            if cursor.is_none() {
                break;
            }
        }
    }
    loop {
        let mut changed = false;
        for (parent, parent_id, child, child_id) in &references {
            if ids.get(parent).is_some_and(|set| set.contains(parent_id)) {
                let set = ids
                    .get_mut(child)
                    .ok_or_else(|| invalid(root, "missing child ID set"))?;
                changed |= set.insert(*child_id);
            }
        }
        if !changed {
            break;
        }
    }
    for mutation in mutations {
        mutation.deletes = ids
            .remove(&mutation.name)
            .ok_or_else(|| invalid(root, "missing cascade ID set"))?
            .into_iter()
            .collect();
    }
    Ok(())
}

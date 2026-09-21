//! Split-aware keyed-population model for ZE-47.
//!
//! A leaf or branch split relocates physical cells. It may never change the
//! logical key set, the ascending key order, an allocated identity, or an
//! installed revision. This module holds that expectation independently: it
//! never imports the engine and never decodes a physical page.

use super::Difference;
use std::collections::{BTreeMap, BTreeSet};

/// One keyed directory entry exactly as an actual observation carries it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyedNode {
    /// Application-key bytes.
    pub key: Vec<u8>,
    /// Identity the store allocated for that key.
    pub node: u128,
    /// Installed revision of the record.
    pub revision: u64,
}

/// Bounded independent expectation over one keyed node population.
#[derive(Clone, Debug, Default)]
pub struct KeyedModel {
    entries: BTreeMap<Vec<u8>, (u128, u64)>,
    order: Vec<Vec<u8>>,
}

fn difference(path: &str, expected: impl ToString, observed: impl ToString) -> Difference {
    Difference {
        path: path.to_owned(),
        expected: expected.to_string(),
        observed: observed.to_string(),
    }
}

impl KeyedModel {
    /// A new empty population.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one created key. A repeated key is a model error, not a silent
    /// overwrite: the engine must never resurrect a key through a create.
    pub fn create(&mut self, key: &[u8], revision: u64) -> Result<(), Difference> {
        if self.entries.contains_key(key) {
            return Err(difference("keyed.create", "absent key", "repeated key"));
        }
        self.entries.insert(key.to_vec(), (0, revision));
        self.order.push(key.to_vec());
        Ok(())
    }

    /// Number of modelled keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no key has been created.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The modelled keys in ascending key order, which is the order a split
    /// directory must still present.
    #[must_use]
    pub fn keys_in_order(&self) -> Vec<Vec<u8>> {
        self.entries.keys().cloned().collect()
    }

    /// Checks one observed population against the model. Identities are
    /// allocated by the store, so the model checks that every identity is
    /// present exactly once and strictly increases with creation order rather
    /// than predicting a value.
    pub fn check(&self, observed: &[KeyedNode]) -> Result<(), Difference> {
        if observed.len() != self.order.len() {
            return Err(difference("keyed.len", self.order.len(), observed.len()));
        }
        let mut identities = BTreeSet::new();
        let mut previous: Option<u128> = None;
        for (index, entry) in observed.iter().enumerate() {
            let expected_key = self
                .order
                .get(index)
                .ok_or_else(|| difference("keyed.order", "modelled key", "missing"))?;
            if &entry.key != expected_key {
                return Err(difference(
                    &format!("keyed[{index}].key"),
                    String::from_utf8_lossy(expected_key),
                    String::from_utf8_lossy(&entry.key),
                ));
            }
            let (_, revision) = self
                .entries
                .get(&entry.key)
                .ok_or_else(|| difference("keyed.member", "modelled key", "unknown key"))?;
            if entry.revision != *revision {
                return Err(difference(
                    &format!("keyed[{index}].revision"),
                    revision,
                    entry.revision,
                ));
            }
            if entry.node == 0 {
                return Err(difference(
                    &format!("keyed[{index}].node"),
                    "nonzero identity",
                    entry.node,
                ));
            }
            if !identities.insert(entry.node) {
                return Err(difference(
                    &format!("keyed[{index}].node"),
                    "unique identity",
                    entry.node,
                ));
            }
            if previous.is_some_and(|earlier| earlier >= entry.node) {
                return Err(difference(
                    &format!("keyed[{index}].node"),
                    "identity above its predecessor",
                    entry.node,
                ));
            }
            previous = Some(entry.node);
        }
        Ok(())
    }

    /// Checks that a retained pre-split root still answers exactly what the
    /// current root answered for the same keys. A split that loses, moves or
    /// rewrites a key is caught here even when both populations are
    /// individually well formed.
    pub fn check_retained(
        &self,
        current: &[KeyedNode],
        retained: &[KeyedNode],
    ) -> Result<(), Difference> {
        self.check(current)?;
        self.check(retained)?;
        for (index, (left, right)) in current.iter().zip(retained.iter()).enumerate() {
            if left != right {
                return Err(difference(
                    &format!("keyed.retained[{index}]"),
                    format!("{}/{}", left.node, left.revision),
                    format!("{}/{}", right.node, right.revision),
                ));
            }
        }
        Ok(())
    }
}

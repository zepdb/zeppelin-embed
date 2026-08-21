//! Segment-local UTF-8 dictionaries and automatically widened row codes.

use std::collections::HashMap;

/// A typed failure while constructing a string dictionary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DictionaryError {
    /// The dictionary cardinality exceeded the u32 code space.
    CardinalityOverflow,
    /// UTF-8 bytes or offsets exceeded the u32 side-array representation.
    StringStorageOverflow,
}

impl std::fmt::Display for DictionaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CardinalityOverflow => formatter.write_str("dictionary cardinality exceeds u32"),
            Self::StringStorageOverflow => formatter.write_str("string storage exceeds u32"),
        }
    }
}

impl std::error::Error for DictionaryError {}

/// The physical width selected for a dictionary-code array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeWidth {
    /// Sixteen-bit codes for at most `u16::MAX` distinct values.
    U16,
    /// Thirty-two-bit codes for larger dictionaries.
    U32,
}

/// A contiguous dictionary-code array selected at segment seal time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DictionaryCodes {
    /// Sixteen-bit row codes.
    U16(Vec<u16>),
    /// Thirty-two-bit row codes.
    U32(Vec<u32>),
}

impl DictionaryCodes {
    /// Returns the physical code width.
    #[must_use]
    pub const fn width(&self) -> CodeWidth {
        match self {
            Self::U16(_) => CodeWidth::U16,
            Self::U32(_) => CodeWidth::U32,
        }
    }

    /// Returns the number of row codes.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U16(codes) => codes.len(),
            Self::U32(codes) => codes.len(),
        }
    }

    /// Returns whether there are no row codes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads one row code without exposing its physical width.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<u32> {
        match self {
            Self::U16(codes) => codes.get(row).copied().map(u32::from),
            Self::U32(codes) => codes.get(row).copied(),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        enum Iter<'a> {
            U16(std::slice::Iter<'a, u16>),
            U32(std::slice::Iter<'a, u32>),
        }

        impl Iterator for Iter<'_> {
            type Item = u32;

            fn next(&mut self) -> Option<Self::Item> {
                match self {
                    Iter::U16(values) => values.next().copied().map(u32::from),
                    Iter::U32(values) => values.next().copied(),
                }
            }
        }

        match self {
            Self::U16(values) => Iter::U16(values.iter()),
            Self::U32(values) => Iter::U32(values.iter()),
        }
    }

    pub(crate) fn from_u32(codes: Vec<u32>, cardinality: u64) -> Result<Self, DictionaryError> {
        match code_width_for_cardinality(cardinality)? {
            CodeWidth::U16 => {
                let narrowed = codes
                    .into_iter()
                    .map(u16::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| DictionaryError::CardinalityOverflow)?;
                Ok(Self::U16(narrowed))
            }
            CodeWidth::U32 => Ok(Self::U32(codes)),
        }
    }
}

pub(crate) fn code_width_for_cardinality(cardinality: u64) -> Result<CodeWidth, DictionaryError> {
    if cardinality <= u64::from(u16::MAX) {
        Ok(CodeWidth::U16)
    } else if cardinality <= u64::from(u32::MAX) {
        Ok(CodeWidth::U32)
    } else {
        Err(DictionaryError::CardinalityOverflow)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct StringStorage {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl StringStorage {
    pub(crate) fn new() -> Self {
        Self {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    pub(crate) fn can_push(&self, value: &str) -> Result<(), DictionaryError> {
        let new_length = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(DictionaryError::StringStorageOverflow)?;
        u32::try_from(new_length)
            .map(|_| ())
            .map_err(|_| DictionaryError::StringStorageOverflow)
    }

    pub(crate) fn push(&mut self, value: &str) -> Result<(), DictionaryError> {
        self.can_push(value)?;
        self.bytes.extend_from_slice(value.as_bytes());
        let offset =
            u32::try_from(self.bytes.len()).map_err(|_| DictionaryError::StringStorageOverflow)?;
        self.offsets.push(offset);
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub(crate) fn get(&self, position: usize) -> Option<&str> {
        let start = self
            .offsets
            .get(position)
            .copied()
            .map(|value| value as usize)?;
        let end = self
            .offsets
            .get(position.saturating_add(1))
            .copied()
            .map(|value| value as usize)?;
        let bytes = self.bytes.get(start..end)?;
        std::str::from_utf8(bytes).ok()
    }
}

/// An immutable segment-local UTF-8 dictionary.
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct StringDictionary {
    values: StringStorage,
}

impl StringDictionary {
    /// Returns the number of distinct values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the dictionary is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Decodes one dictionary code.
    #[must_use]
    pub fn get(&self, code: u32) -> Option<&str> {
        let position = usize::try_from(code).ok()?;
        self.values.get(position)
    }

    /// Finds the code assigned to a string value.
    #[must_use]
    pub fn find(&self, needle: &str) -> Option<u32> {
        self.values
            .offsets
            .windows(2)
            .enumerate()
            .find_map(|(position, _)| {
                (self.values.get(position) == Some(needle))
                    .then(|| u32::try_from(position).ok())
                    .flatten()
            })
    }
}

pub(crate) struct DictionaryBuilder {
    lookup: HashMap<String, u32>,
    values: StringStorage,
}

impl DictionaryBuilder {
    pub(crate) fn new() -> Self {
        Self {
            lookup: HashMap::new(),
            values: StringStorage::new(),
        }
    }

    pub(crate) fn can_intern(&self, value: &str) -> Result<(), DictionaryError> {
        if self.lookup.contains_key(value) {
            return Ok(());
        }
        let cardinality = u64::try_from(self.lookup.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        code_width_for_cardinality(cardinality)?;
        self.values.can_push(value)
    }

    pub(crate) fn intern(&mut self, value: &str) -> Result<u32, DictionaryError> {
        if let Some(code) = self.lookup.get(value).copied() {
            return Ok(code);
        }
        self.can_intern(value)?;
        let code =
            u32::try_from(self.lookup.len()).map_err(|_| DictionaryError::CardinalityOverflow)?;
        self.values.push(value)?;
        self.lookup.insert(value.to_owned(), code);
        Ok(code)
    }

    pub(crate) fn cardinality(&self) -> u64 {
        u64::try_from(self.lookup.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn finish(self) -> StringDictionary {
        StringDictionary {
            values: self.values,
        }
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;

    #[test]
    fn prop_dict_roundtrip() {
        let name = "meta::dict::prop_dict_roundtrip";
        let mut seeded = crate::test_support::seeded_rng(name);
        let config = Config {
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        };
        let mut runner = TestRunner::new(config);
        let values = prop::collection::vec("[a-z]{0,16}", 0..=256);
        let result = runner.run(&values, |values| {
            let mut builder = DictionaryBuilder::new();
            let codes = values
                .iter()
                .map(|value| builder.intern(value).expect("bounded dictionary"))
                .collect::<Vec<_>>();
            let dictionary = builder.finish();
            let decoded = codes
                .into_iter()
                .map(|code| dictionary.get(code).map(str::to_owned))
                .collect::<Vec<_>>();
            let expected = values.into_iter().map(Some).collect::<Vec<_>>();
            prop_assert_eq!(decoded, expected);
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }

    #[test]
    fn dictionary_auto_widens_u16_to_u32() {
        let cardinality = u64::from(u16::MAX).saturating_add(1);
        let codes = (0..=u32::from(u16::MAX)).collect::<Vec<_>>();
        let encoded = DictionaryCodes::from_u32(codes, cardinality).expect("u32 cardinality");
        assert_eq!(encoded.width(), CodeWidth::U32);
    }

    #[test]
    fn dictionary_reports_u32_cardinality_overflow() {
        assert_eq!(
            code_width_for_cardinality(u64::from(u32::MAX).saturating_add(1)),
            Err(DictionaryError::CardinalityOverflow)
        );
    }
}

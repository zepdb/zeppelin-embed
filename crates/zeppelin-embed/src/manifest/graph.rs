use super::{ManifestCursor, ManifestError, append_u32_len};
use crate::property_graph::storage::artifact::{ArtifactId, MAX_ARTIFACT_BYTES};
use crate::property_graph::wal::{
    CommitState, STACK_RESERVATION_BYTES, WalResources, commit_state_size, decode_commit_state,
    encode_commit_state,
};
use std::collections::BTreeMap;
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 4] = b"ZGR3";
const GENERATION_BOUNDARY_V2: &[u8; 8] = b"ZGEN\x02\x00\x00\x00";
const GENERATION_BOUNDARY_V1: &[u8; 8] = b"ZGEN\x01\x00\x00\x00";

/// One live immutable .zgraph object, identified independently of its directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphObject {
    /// Stable object nonce used in its relative file name.
    pub artifact: ArtifactId,
    /// Complete immutable file length.
    pub length: u64,
    /// Whole-file trailer checksum.
    pub checksum: u64,
}

/// Validated ZE-38 state and the complete live graph object inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphManifest {
    state: Vec<u8>,
    /// Last graph WAL record absorbed by this manifest.
    pub graph_absorbed_through: u64,
    /// WAL boundary already counted in the manifest generation without
    /// absorbing the corresponding document or graph WAL records.
    pub generation_absorbed_through: Option<u64>,
    /// Manifest-only generation increments, grouped by the WAL boundary after
    /// which they happened. Rows are strictly ordered (sequence, increment count).
    pub generation_bumps: Vec<(u64, u64)>,
    /// Live .zgraph objects; no paths are persisted.
    pub objects: Vec<GraphObject>,
}

fn wal_error(error: crate::property_graph::wal::WalError) -> ManifestError {
    ManifestError::Decode(error.to_string())
}

impl GraphManifest {
    /// Copies an admitted CommitState using its existing wire codec.
    /// Graph text/vector roots remain participants until document projection lands.
    pub fn new(
        state: CommitState<'_>,
        graph_absorbed_through: u64,
        objects: Vec<GraphObject>,
    ) -> Result<Self, ManifestError> {
        let mut cancel = || false;
        let mut resources =
            WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancel).map_err(wal_error)?;
        let length = commit_state_size(state, &mut resources).map_err(wal_error)?;
        let mut bytes = vec![0; length];
        encode_commit_state(state, &mut bytes, &mut resources).map_err(wal_error)?;
        let graph = Self {
            state: bytes,
            graph_absorbed_through,
            generation_absorbed_through: None,
            generation_bumps: Vec::new(),
            objects,
        };
        graph.validate()?;
        Ok(graph)
    }

    pub(crate) fn backing_bytes(&self) -> Option<usize> {
        self.state
            .capacity()
            .checked_add(self.generation_bumps.capacity().checked_mul(16)?)?
            .checked_add(
                self.objects
                    .capacity()
                    .checked_mul(std::mem::size_of::<GraphObject>())?,
            )
    }

    /// Borrows the typed state without introducing another persisted codec.
    pub fn state(&self) -> Result<CommitState<'_>, ManifestError> {
        let mut cancel = || false;
        let mut resources =
            WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancel).map_err(wal_error)?;
        decode_commit_state(&self.state, &mut resources).map_err(wal_error)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let mut previous = None;
        for &(sequence, count) in &self.generation_bumps {
            if count == 0
                || previous.is_some_and(|prior| prior >= sequence)
                || self
                    .generation_absorbed_through
                    .is_none_or(|boundary| sequence > boundary)
            {
                return Err(ManifestError::Decode(
                    "invalid graph generation bumps".to_owned(),
                ));
            }
            previous = Some(sequence);
        }
        let state = self.state()?;
        for reference in state.vector.into_iter().chain(state.text) {
            if reference.block.kind
                != crate::property_graph::storage::artifact::BlockKind::CommitParticipant
            {
                return Err(ManifestError::Decode(
                    "invalid graph search root role".to_owned(),
                ));
            }
        }
        let mut inventory = BTreeMap::new();
        for object in &self.objects {
            if object.length < 104
                || object.length > MAX_ARTIFACT_BYTES as u64
                || inventory.insert(object.artifact, object).is_some()
            {
                return Err(ManifestError::Decode(
                    "invalid or duplicate graph object".to_owned(),
                ));
            }
        }
        let check = |reference: crate::property_graph::wal::RequiredRef| match inventory
            .get(&reference.object.artifact)
        {
            Some(object)
                if object.length == u64::from(reference.object.bytes)
                    && object.checksum == reference.object.checksum =>
            {
                Ok(())
            }
            _ => Err(ManifestError::Decode(
                "graph participant is missing or differs from inventory".to_owned(),
            )),
        };
        for reference in state
            .graph
            .slots
            .into_iter()
            .flatten()
            .chain(Some(state.catalog))
            .chain(state.vector)
            .chain(state.text)
            .chain(state.reclaim)
        {
            check(reference)?;
        }
        let mut cancel = || false;
        let mut resources =
            WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancel).map_err(wal_error)?;
        for index in 0..state.prepared_inventories.len().map_err(wal_error)? {
            check(
                state
                    .prepared_inventories
                    .get(index, &mut resources)
                    .map_err(wal_error)?,
            )?;
        }
        Ok(())
    }
}

pub(super) fn append(graph: &GraphManifest, output: &mut Vec<u8>) -> Result<(), ManifestError> {
    graph.validate()?;
    let start = output.len();
    output.extend_from_slice(MAGIC);
    let length =
        24_usize
            .checked_add(if graph.generation_absorbed_through.is_some() {
                if graph.generation_bumps.is_empty() {
                    16
                } else {
                    24_usize
                        .checked_add(graph.generation_bumps.len().checked_mul(16).ok_or_else(
                            || ManifestError::Decode("generation bump length overflow".to_owned()),
                        )?)
                        .ok_or_else(|| {
                            ManifestError::Decode("generation bump length overflow".to_owned())
                        })?
                }
            } else {
                0
            })
            .and_then(|size| size.checked_add(graph.state.len()))
            .and_then(|size| {
                graph
                    .objects
                    .len()
                    .checked_mul(32)
                    .and_then(|rows| size.checked_add(rows))
            })
            .ok_or_else(|| ManifestError::Decode("graph section length overflow".to_owned()))?;
    append_u32_len(length, "graph section", output)?;
    output.push(1);
    output.extend_from_slice(&[0; 7]);
    output.extend_from_slice(&graph.graph_absorbed_through.to_le_bytes());
    append_u32_len(graph.state.len(), "graph state", output)?;
    output.extend_from_slice(&graph.state);
    append_u32_len(graph.objects.len(), "graph objects", output)?;
    for object in &graph.objects {
        output.extend_from_slice(&object.artifact.get().to_le_bytes());
        output.extend_from_slice(&object.length.to_le_bytes());
        output.extend_from_slice(&object.checksum.to_le_bytes());
    }
    if let Some(sequence) = graph.generation_absorbed_through {
        output.extend_from_slice(if graph.generation_bumps.is_empty() {
            GENERATION_BOUNDARY_V1
        } else {
            GENERATION_BOUNDARY_V2
        });
        output.extend_from_slice(&sequence.to_le_bytes());
        if !graph.generation_bumps.is_empty() {
            append_u32_len(graph.generation_bumps.len(), "generation bumps", output)?;
            output.extend_from_slice(&[0; 4]);
            for (boundary, count) in &graph.generation_bumps {
                output.extend_from_slice(&boundary.to_le_bytes());
                output.extend_from_slice(&count.to_le_bytes());
            }
        }
    }
    let section = output
        .get(start..)
        .ok_or_else(|| ManifestError::Decode("graph section extent".to_owned()))?;
    let checksum = xxh3_64(section);
    output.extend_from_slice(&checksum.to_le_bytes());
    Ok(())
}

pub(super) fn decode(cursor: &mut ManifestCursor<'_>) -> Result<GraphManifest, ManifestError> {
    let start = cursor.position;
    if cursor.take(4)? != MAGIC {
        return Err(ManifestError::Decode(
            "unknown graph section magic".to_owned(),
        ));
    }
    let length = cursor.usize_from_u32()?;
    let body = cursor.take(length)?;
    let checksum_end = cursor.position;
    let checksum = cursor.u64()?;
    let section = cursor
        .bytes
        .get(start..checksum_end)
        .ok_or_else(|| ManifestError::Decode("graph section extent".to_owned()))?;
    if xxh3_64(section) != checksum {
        return Err(ManifestError::Decode(
            "graph section checksum mismatch".to_owned(),
        ));
    }
    let mut graph = ManifestCursor::new(body);
    if graph.u8()? != 1 || graph.take(7)?.iter().any(|byte| *byte != 0) {
        return Err(ManifestError::Decode(
            "invalid graph presence or reserved bytes".to_owned(),
        ));
    }
    let graph_absorbed_through = graph.u64()?;
    let length = graph.usize_from_u32()?;
    let state = graph.take(length)?.to_vec();
    let count = graph.usize_from_u32()?;
    let object_bytes = count.checked_mul(32);
    let extension_bytes = object_bytes.and_then(|bytes| graph.remaining().checked_sub(bytes));
    if extension_bytes
        .is_none_or(|extra| extra != 0 && extra != 16 && (extra < 40 || (extra - 24) % 16 != 0))
    {
        return Err(ManifestError::Decode(
            "graph object count/length mismatch".to_owned(),
        ));
    }
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
        let bytes = graph
            .take(16)?
            .try_into()
            .map_err(|_| ManifestError::Decode("graph artifact id extent".to_owned()))?;
        objects.push(GraphObject {
            artifact: ArtifactId::new(u128::from_le_bytes(bytes))?,
            length: graph.u64()?,
            checksum: graph.u64()?,
        });
    }
    let mut generation_bumps = Vec::new();
    let generation_absorbed_through = if graph.remaining() != 0 {
        let tag = graph.take(8)?;
        if !tag.starts_with(b"ZGEN") {
            return Err(ManifestError::Decode(
                "graph object count/length mismatch".to_owned(),
            ));
        }
        if tag != GENERATION_BOUNDARY_V1 && tag != GENERATION_BOUNDARY_V2 {
            return Err(ManifestError::Decode(
                "unknown graph generation boundary".to_owned(),
            ));
        }
        let boundary = graph.u64()?;
        if tag == GENERATION_BOUNDARY_V2 {
            let count = graph.usize_from_u32()?;
            if graph.u32()? != 0 || count == 0 || count.checked_mul(16) != Some(graph.remaining()) {
                return Err(ManifestError::Decode(
                    "graph generation bump count/length mismatch".to_owned(),
                ));
            }
            for _ in 0..count {
                generation_bumps.push((graph.u64()?, graph.u64()?));
            }
        }
        Some(boundary)
    } else {
        None
    };
    graph.finish()?;
    let decoded = GraphManifest {
        state,
        graph_absorbed_through,
        generation_absorbed_through,
        generation_bumps,
        objects,
    };
    decoded.validate()?;
    Ok(decoded)
}

#[cfg(test)]
#[path = "original_v3_decoder.rs"]
mod original_v3_decoder;

#[cfg(test)]
#[path = "previous_v3_decoder.rs"]
mod previous_v3_decoder;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn fixture() -> GraphManifest {
        let bytes = crate::format::golden::decode_hex(include_str!(
            "../../tests/fixtures/format/manifest_v3.hex"
        ))
        .unwrap();
        crate::manifest::decode_manifest("v3", &bytes)
            .unwrap()
            .graph
            .unwrap()
    }

    #[test]
    fn original_v3_decoder_refuses_the_16_byte_generation_cutoff() {
        let mut graph = fixture();
        let mut bytes = Vec::new();
        append(&graph, &mut bytes).unwrap();
        assert_eq!(
            original_v3_decoder::decode(&mut ManifestCursor::new(&bytes)).unwrap(),
            graph
        );
        graph.generation_absorbed_through = Some(43);
        bytes.clear();
        append(&graph, &mut bytes).unwrap();
        assert!(
            matches!(original_v3_decoder::decode(&mut ManifestCursor::new(&bytes)),
            Err(ManifestError::Decode(message)) if message == "graph object count/length mismatch")
        );
    }

    #[test]
    fn previous_v3_decoder_refuses_manifest_only_generation_bumps() {
        let mut graph = fixture();
        for boundary in [None, Some(43)] {
            graph.generation_absorbed_through = boundary;
            let mut bytes = Vec::new();
            append(&graph, &mut bytes).unwrap();
            assert_eq!(
                previous_v3_decoder::decode(&mut ManifestCursor::new(&bytes)).unwrap(),
                graph
            );
        }
        graph.generation_bumps = vec![(42, 1), (43, 2)];
        let mut bytes = Vec::new();
        append(&graph, &mut bytes).unwrap();
        assert_eq!(decode(&mut ManifestCursor::new(&bytes)).unwrap(), graph);
        assert!(
            matches!(previous_v3_decoder::decode(&mut ManifestCursor::new(&bytes)),
            Err(ManifestError::Decode(message)) if message == "graph object count/length mismatch")
        );
    }

    #[test]
    fn generation_bumps_refuse_zero_counts_unordered_rows_and_unknown_tags() {
        let mut graph = fixture();
        graph.generation_absorbed_through = Some(43);
        for rows in [vec![(42, 0)], vec![(43, 1), (42, 1)], vec![(44, 1)]] {
            graph.generation_bumps = rows;
            assert!(append(&graph, &mut Vec::new()).is_err());
        }
        graph.generation_bumps = vec![(42, 1)];
        let mut bytes = Vec::new();
        append(&graph, &mut bytes).unwrap();
        let tag = bytes
            .windows(8)
            .position(|bytes| bytes == GENERATION_BOUNDARY_V2)
            .unwrap();
        bytes[tag + 4] = 3;
        let end = bytes.len() - 8;
        let checksum = xxh3_64(&bytes[..end]);
        bytes[end..].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(decode(&mut ManifestCursor::new(&bytes)),
            Err(ManifestError::Decode(message)) if message == "unknown graph generation boundary"));
    }
}

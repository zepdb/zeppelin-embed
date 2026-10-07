use super::{ManifestCursor, ManifestError, append_u32_len};
use crate::property_graph::storage::artifact::{ArtifactId, MAX_ARTIFACT_BYTES};
use crate::property_graph::wal::{
    CommitState, STACK_RESERVATION_BYTES, WalResources, commit_state_size, decode_commit_state,
    encode_commit_state,
};
use std::collections::BTreeMap;
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 4] = b"ZGR3";

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
            objects,
        };
        graph.validate()?;
        Ok(graph)
    }

    pub(crate) fn backing_bytes(&self) -> Option<usize> {
        self.state.capacity().checked_add(
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
    let length = 24_usize
        .checked_add(graph.state.len())
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
    if count.checked_mul(32) != Some(graph.remaining()) {
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
    graph.finish()?;
    let decoded = GraphManifest {
        state,
        graph_absorbed_through,
        objects,
    };
    decoded.validate()?;
    Ok(decoded)
}

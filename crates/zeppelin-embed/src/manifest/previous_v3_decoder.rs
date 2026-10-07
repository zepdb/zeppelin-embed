// Frozen 96fbee0a graph-section decoder. The new in-memory field defaults empty;
// all byte reads and refusal checks are unchanged. This module is test-only.
use super::{
    GENERATION_BOUNDARY_V1, GraphManifest, GraphObject, MAGIC, ManifestCursor, ManifestError,
};
use crate::property_graph::storage::artifact::ArtifactId;
use xxhash_rust::xxh3::xxh3_64;

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
    let has_generation_boundary =
        object_bytes.and_then(|bytes| bytes.checked_add(16)) == Some(graph.remaining());
    if object_bytes != Some(graph.remaining()) && !has_generation_boundary {
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
    let generation_absorbed_through = if has_generation_boundary {
        if graph.take(8)? != GENERATION_BOUNDARY_V1 {
            return Err(ManifestError::Decode(
                "unknown graph generation boundary".to_owned(),
            ));
        }
        Some(graph.u64()?)
    } else {
        None
    };
    graph.finish()?;
    let decoded = GraphManifest {
        state,
        graph_absorbed_through,
        generation_absorbed_through,
        generation_bumps: Vec::new(),
        objects,
    };
    decoded.validate()?;
    Ok(decoded)
}

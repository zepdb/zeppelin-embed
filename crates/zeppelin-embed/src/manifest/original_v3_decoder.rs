// Frozen pre-ZGEN 5e3b9cb9 decoder. Only new in-memory fields default empty;
// its byte reads and refusal checks remain unchanged. Test-only.
use super::{GraphManifest, GraphObject, MAGIC, ManifestCursor, ManifestError};
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
        generation_absorbed_through: None,
        generation_bumps: Vec::new(),
        objects,
    };
    decoded.validate()?;
    Ok(decoded)
}

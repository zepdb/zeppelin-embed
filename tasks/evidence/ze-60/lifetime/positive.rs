use crate::ingest::DocumentVersion;
use crate::property_graph::query::runtime::RuntimeContext;
use crate::property_graph::retrieval::{NativeRetrievalContext, RetrievalError};

fn copy_payload_inside_admission<'view, 's, 'lease, 'm, 'g>(
    context: &NativeRetrievalContext<'view, 's, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    expected: DocumentVersion,
) -> Result<(DocumentVersion, Option<Vec<u8>>, Option<Vec<u32>>), RetrievalError> {
    let resolved = context.resolve(expected, runtime)?;
    let version = resolved.version();
    let text = context
        .copy_text(&resolved, runtime)?
        .map(|bytes| bytes.as_slice().to_vec());
    let vector = context.copy_vector(&resolved, runtime)?.map(|coordinates| {
        coordinates
            .as_slice()
            .iter()
            .map(|coordinate| coordinate.to_bits())
            .collect()
    });
    Ok((version, text, vector))
}

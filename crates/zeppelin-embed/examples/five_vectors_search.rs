use std::error::Error;

use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};

const VECTORS: [[f32; 4]; 5] = [
    [0.90, 0.10, 0.05, 0.00],
    [0.85, 0.15, 0.10, 0.05],
    [0.10, 0.90, 0.05, 0.00],
    [0.05, 0.10, 0.90, 0.00],
    [0.00, 0.05, 0.10, 0.90],
];

fn main() -> Result<(), Box<dyn Error>> {
    // A store is a persistent directory. This example uses a unique temporary
    // path so repeated runs do not share data.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("zeppelin-rust-example-{nonce}"));
    let store = Store::open(&path, OpenOptions::default())?;

    // Zeppelin Embed accepts caller-supplied vectors. Every document also has
    // a stable ID and a revision used to order later updates.
    let documents = VECTORS
        .into_iter()
        .enumerate()
        .map(|(index, vector)| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new((index + 1) as u128), Revision::new(1)),
                vector.to_vec(),
            )
            .with_timestamp((index as i64 + 1) * 10)
        })
        .collect();
    // The batch becomes visible together in the generation named by the ack.
    let ack = store.ingest(IngestBatch::new(documents))?;
    println!("ingested 5 vectors at generation {}", ack.generation());

    // Query vectors must use the same dimension and embedding space as the
    // document vectors. Search returns the three nearest candidates.
    let query = [0.88, 0.12, 0.07, 0.02];
    let result = store.search(
        SearchRequest::new(&query),
        3,
        SearchOptions::default(),
        QueryControl::Cancel(CancelToken::new()),
    )?;
    for (rank, hit) in result.candidates.iter().enumerate() {
        let id = hit
            .document()
            .map(|document| document.doc_id().get())
            .unwrap_or(0);
        println!("{}. document {}, score {:.6}", rank + 1, id, hit.score());
    }

    // Close releases the store lock before removing this example's data.
    store.close()?;
    std::fs::remove_dir_all(path)?;
    Ok(())
}

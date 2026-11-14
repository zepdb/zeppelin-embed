//! Compare no-preference dense search with Exact on raw little-endian f32 rows.
use std::{error::Error, fs, time::Instant};
use zeppelin_embed::{
    ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest},
    lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store},
};

fn rows(path: &str, dims: usize) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    if dims == 0 || bytes.len() % (dims * 4) != 0 {
        return Err("raw f32 file has an incomplete row".into());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

fn percentile(samples: &mut [f64], percent: usize) -> f64 {
    samples.sort_unstable_by(f64::total_cmp);
    samples[(samples.len() - 1) * percent / 100]
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 5 {
        return Err(
            "usage: default-dense-cost <vectors> <queries> <dims> <queries_n> <rounds>".into(),
        );
    }
    let dims: usize = args[2].parse()?;
    let query_count: usize = args[3].parse()?;
    let rounds: usize = args[4].parse()?;
    if query_count == 0 || rounds == 0 {
        return Err("queries_n and rounds must be positive".into());
    }
    let vectors = rows(&args[0], dims)?;
    let queries = rows(&args[1], dims)?;
    if queries.len() / dims < query_count {
        return Err("not enough query rows".into());
    }
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    for (batch, values) in vectors.chunks(10_000 * dims).enumerate() {
        let documents = values
            .chunks_exact(dims)
            .enumerate()
            .map(|(row, vector)| {
                IngestDocument::new(
                    DocumentVersion::new(
                        DocId::new((batch * 10_000 + row + 1) as u128),
                        Revision::new(1),
                    ),
                    vector.to_vec(),
                )
            })
            .collect();
        store.ingest(IngestBatch::new(documents))?;
    }
    store.seal()?;
    println!(
        "rows={} dims={dims} queries={query_count} rounds={rounds} k=10",
        vectors.len() / dims
    );
    drop(vectors);
    let mut timings = [Vec::new(), Vec::new()];
    let mut overlap = 0;
    let mut ordered = 0;
    let mut bits = 0;
    let mut bytes = [0_u64; 2];
    let mut exact_rescore = true;
    let mut approximate = false;
    for round in 0..rounds {
        for (index, query) in queries.chunks_exact(dims).take(query_count).enumerate() {
            let mut results = [None, None];
            // Alternate which tier leads, while keeping each pair adjacent.
            for arm in [(round + index) % 2, (round + index + 1) % 2] {
                let options = if arm == 0 {
                    SearchOptions::default()
                } else {
                    SearchOptions::default().with_tier(SearchTier::Exact)
                };
                let start = Instant::now();
                let result = store.search(
                    SearchRequest::new(query),
                    10,
                    options,
                    QueryControl::Cancel(CancelToken::new()),
                )?;
                timings[arm].push(start.elapsed().as_secs_f64() * 1e6);
                if round == 0 {
                    bytes[arm] += result.diagnostics.counters.scan.bytes_read;
                    if arm == 0 {
                        exact_rescore &= result.diagnostics.exact_rescore;
                        approximate |= result.diagnostics.approximate;
                    }
                }
                results[arm] = Some(result.candidates);
            }
            if round == 0 {
                let default = results[0].as_ref().ok_or("missing default arm")?;
                let exact = results[1].as_ref().ok_or("missing Exact arm")?;
                let ids = |hits: &Vec<zeppelin_embed::ingest::SearchCandidate>| {
                    hits.iter().map(|hit| hit.document()).collect::<Vec<_>>()
                };
                let default_ids = ids(default);
                let exact_ids = ids(exact);
                overlap += default_ids
                    .iter()
                    .filter(|id| exact_ids.contains(id))
                    .count();
                ordered += usize::from(default_ids == exact_ids);
                bits += default
                    .iter()
                    .zip(exact)
                    .filter(|(a, b)| a.score().to_bits() == b.score().to_bits())
                    .count();
            }
        }
    }
    println!(
        "recall@10={:.6} ordered_equal={ordered}/{query_count} score_bits_equal={bits}/{} exact_rescore={exact_rescore} approximate={approximate}",
        overlap as f64 / (query_count * 10) as f64,
        query_count * 10
    );
    for arm in 0..2 {
        let name = if arm == 0 { "default" } else { "Exact" };
        let p50 = percentile(&mut timings[arm], 50);
        let p95 = percentile(&mut timings[arm], 95);
        println!(
            "tier={name} bytes_read={} p50_us={p50:.3} p95_us={p95:.3}",
            bytes[arm]
        );
    }
    store.close()?;
    Ok(())
}

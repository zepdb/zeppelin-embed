//! Deterministic two-arm publication measurement: <segments> <rows> <reps>.
use std::error::Error;
use std::io;
use std::time::Instant;

use zeppelin_embed::fts::{index::DEFAULT_FIELD, search::TermQuery};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};

struct Corpus {
    state: u64,
    next_id: u128,
}

impl Corpus {
    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn document(&mut self) -> IngestDocument {
        let vector = (0..64)
            .map(|_| (self.next() >> 40) as f32 / 16_777_216.0 - 0.5)
            .collect();
        let text = (0..32)
            .map(|_| format!("word{:04}", (self.next() >> 16) % 5000))
            .collect::<Vec<_>>()
            .join(" ");
        let id = self.next_id;
        self.next_id += 1;
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vector,
        )
        .with_text(text)
    }
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn lexical(store: &Store, terms: &TermQuery) -> Result<Vec<u128>, Box<dyn Error>> {
    Ok(store
        .search_lexical(terms, 10, control())?
        .candidates
        .into_iter()
        .map(|hit| hit.document.doc_id().get())
        .collect())
}

fn hybrid(store: &Store, terms: &TermQuery, vector: &[f32]) -> Result<Vec<u128>, Box<dyn Error>> {
    Ok(store
        .search_hybrid(
            SearchRequest::new(vector),
            terms,
            &HybridQuery::new(10),
            SearchOptions::default(),
            control(),
        )?
        .hits
        .into_iter()
        .map(|hit| hit.key.get())
        .collect())
}

fn argument(args: &mut impl Iterator<Item = String>) -> Result<usize, Box<dyn Error>> {
    let value = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: ze540_reload <segments> <rows> <reps>",
        )
    })?;
    let value = value.parse::<usize>()?;
    if value == 0 {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "arguments must be positive").into(),
        );
    }
    Ok(value)
}

fn median(values: &mut [u128]) -> Result<u128, io::Error> {
    values.sort_unstable();
    values
        .get(values.len() / 2)
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no samples"))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let segments = argument(&mut args)?;
    let rows = argument(&mut args)?;
    let reps = argument(&mut args)?;
    if args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unexpected argument").into());
    }
    let directory = tempfile::tempdir()?;
    println!(
        "segments={segments} rows={rows} reps={reps} dims=64 words_per_text=32 vocabulary=5000 seed=0x540 dataset={}",
        directory.path().display()
    );
    let store = Store::open(directory.path(), OpenOptions::default())?;
    let mut corpus = Corpus {
        state: 0x540,
        next_id: 1,
    };
    for _ in 0..segments {
        store.ingest(IngestBatch::new(
            (0..rows).map(|_| corpus.document()).collect(),
        ))?;
        store.seal()?;
    }
    let terms = TermQuery::flat(
        vec![
            b"word0001".to_vec(),
            b"word0250".to_vec(),
            b"word4999".to_vec(),
        ],
        &[DEFAULT_FIELD],
    );
    let vector = (0..64)
        .map(|i| (i as f32 - 32.0) / 64.0)
        .collect::<Vec<_>>();
    println!("warm_lexical_ids={:?}", lexical(&store, &terms)?);
    let dense = store.search(
        SearchRequest::new(&vector),
        10,
        SearchOptions::default(),
        control(),
    )?;
    println!(
        "warm_dense_ids={:?}",
        dense
            .candidates
            .into_iter()
            .filter_map(|hit| hit.document())
            .map(|document| document.doc_id().get())
            .collect::<Vec<_>>()
    );
    println!("warm_hybrid_ids={:?}", hybrid(&store, &terms, &vector)?);
    let mut seals = Vec::with_capacity(reps);
    let mut lexicals = Vec::with_capacity(reps);
    let mut hybrids = Vec::with_capacity(reps);
    for rep in 0..reps {
        store.ingest(IngestBatch::new(vec![corpus.document()]))?;
        let start = Instant::now();
        store.seal()?;
        let seal_us = start.elapsed().as_micros();
        let start = Instant::now();
        let lexical_ids = lexical(&store, &terms)?;
        let lexical_us = start.elapsed().as_micros();
        store.ingest(IngestBatch::new(vec![corpus.document()]))?;
        store.seal()?;
        let start = Instant::now();
        let hybrid_ids = hybrid(&store, &terms, &vector)?;
        let hybrid_us = start.elapsed().as_micros();
        println!(
            "rep={rep} seal_us={seal_us} lexical_us={lexical_us} hybrid_us={hybrid_us} lexical_ids={lexical_ids:?} hybrid_ids={hybrid_ids:?}"
        );
        seals.push(seal_us);
        lexicals.push(lexical_us);
        hybrids.push(hybrid_us);
    }
    println!(
        "median_us seal={} lexical={} hybrid={}",
        median(&mut seals)?,
        median(&mut lexicals)?,
        median(&mut hybrids)?
    );
    store.close()?;
    Ok(())
}

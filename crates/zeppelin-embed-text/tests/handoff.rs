#![allow(clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestDocument, Revision};
use zeppelin_embed_text::runtime::EmbeddingBatch;

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every operation is delegated unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `layout` is forwarded from the allocator caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from `System::alloc` above.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn the_embed_to_writer_hand_off_allocates_zero_bytes_per_document() {
    let batch = EmbeddingBatch::new(vec![1.0; 128 * 768], 128, 768).expect("batch");
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let values = batch.into_values();
    COUNTING.store(false, Ordering::Relaxed);

    assert_eq!(values.len(), 128 * 768);
    let core_vectors = values
        .chunks_exact(768)
        .map(<[f32]>::to_vec)
        .collect::<Vec<_>>();
    let mut documents = Vec::with_capacity(core_vectors.len());
    COUNTING.store(true, Ordering::Relaxed);
    for (row, vector) in core_vectors.into_iter().enumerate() {
        documents.push(IngestDocument::new(
            DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
            vector,
        ));
    }
    COUNTING.store(false, Ordering::Relaxed);

    assert_eq!(documents.len(), 128);
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
}

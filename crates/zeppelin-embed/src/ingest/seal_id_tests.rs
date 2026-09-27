#![allow(clippy::expect_used, clippy::panic)]

use tempfile::tempdir;

use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{OpenOptions, Store};

#[derive(Clone, Copy, Debug)]
enum Op {
    Put(u8),
    Delete(u8),
    Seal,
}

fn replay(ops: &[Op]) {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let mut revisions = [0_u64; 256];
    for (index, op) in ops.iter().enumerate() {
        match *op {
            Op::Put(id) => {
                let revision = revisions.get_mut(usize::from(id)).expect("slot");
                *revision += 1;
                store
                    .ingest(IngestBatch::new(vec![
                        IngestDocument::new(
                            DocumentVersion::new(
                                DocId::new(u128::from(id)),
                                Revision::new(*revision),
                            ),
                            vec![f32::from(id)],
                        )
                        .with_timestamp(0),
                    ]))
                    .unwrap_or_else(|error| panic!("op {index} {op:?}: {error:?}"));
            }
            Op::Delete(id) => {
                store
                    .delete(DeleteBatch::new(vec![DocId::new(u128::from(id))]))
                    .unwrap_or_else(|error| panic!("op {index} {op:?}: {error:?}"));
            }
            Op::Seal => {
                store.seal().expect("seal");
            }
        }
    }
}

#[test]
fn repeated_seals_after_sealed_replacements_never_reuse_a_segment_id() {
    use Op::{Delete, Put, Seal};
    let mut ops = vec![Put(0), Put(0), Put(0), Seal, Put(0), Put(0), Put(0), Put(4)];
    ops.extend([Put(0), Put(0), Put(0), Seal, Put(0), Seal]);
    ops.extend([Put(0); 6]);
    ops.extend([Put(3), Put(11), Delete(4), Put(17), Put(20), Seal]);
    ops.extend([Put(2), Put(3), Put(12), Put(22), Put(18), Seal, Put(2)]);
    replay(&ops);
}

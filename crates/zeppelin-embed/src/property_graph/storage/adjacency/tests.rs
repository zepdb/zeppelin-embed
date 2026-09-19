#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use crate::allocation_audit::audit_engine_path;
#[test]
fn adjacency_actual_allocator_reports_zero_for_encode_merge_and_all_error_classes() {
    let k = RangeKey {
        node: NodeId::new(1).unwrap(),
        rel_type: RelTypeId::new(2).unwrap(),
        direction: Direction::Out,
        lower: RelId::new(1).unwrap(),
        upper: UpperBound::Infinity,
    };
    let edge = Edge {
        rel: RelId::new(u128::MAX).unwrap(),
        neighbor: NodeId::new(2).unwrap(),
    };
    let entries = [edge];
    let mut base = [0; 128];
    let mut delta = [0; 136];
    let mut output = [edge; 1];
    let (result, audit) = audit_engine_path(|| {
        let b = encode_base(k, 0, &entries, &mut base, &mut |_| Ok::<_, u8>(()))?;
        let d = encode_delta(
            k,
            1,
            &[DeltaEntry {
                edge,
                action: Action::Insert,
            }],
            &mut delta,
            &mut |_| Ok::<_, u8>(()),
        )?;
        let m = merge(k, 0, 1, b, &[d], &mut output, &mut |_| Ok::<_, u8>(()))?;
        Ok::<_, Error<u8>>(m.edges().len())
    });
    assert_eq!(result, Ok(1));
    assert_eq!(
        (
            audit.allocations,
            audit.unattributed_bytes,
            audit.attributed_bytes
        ),
        (0, 0, 0)
    );
    base[95] = 1;
    let (result, audit) = audit_engine_path(|| {
        merge(k, 0, 1, &base, &[&delta], &mut output, &mut |_| {
            Ok::<_, u8>(())
        })
        .map(|m| m.edges().len())
    });
    assert_eq!(result, Err(Error::Format(FormatIssue::Reserved)));
    assert_eq!(audit.allocations, 0);
    base[95] = 0;
    let (result, audit) = audit_engine_path(|| {
        merge(k, 0, 1, &base, &[&delta], &mut [], &mut |_| Ok::<_, u8>(())).map(|m| m.edges().len())
    });
    assert_eq!(result, Err(Error::Limit(LimitIssue::Output)));
    assert_eq!(audit.allocations, 0);
    let (result, audit) = audit_engine_path(|| {
        merge(k, 0, 1, &base, &[&delta], &mut output, &mut |w| {
            if w == Work::Finish { Err(7_u8) } else { Ok(()) }
        })
        .map(|m| m.edges().len())
    });
    assert_eq!(result, Err(Error::Control(7)));
    assert_eq!(audit.allocations, 0);
    eprintln!(
        "adjacency caller bytes={} cursor/run representation={} allocations=0 (success,format,limit,final-control)",
        base.len() + delta.len() + std::mem::size_of_val(&output),
        MERGE_STATE_BYTES
    );
}

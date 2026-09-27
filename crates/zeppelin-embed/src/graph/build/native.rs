use super::{
    BuildControl, BuildMemoryEvent, GraphBuildArtifact, GraphBuildError, SegmentVectors,
    build_native_graph_inner,
};
use crate::graph::GraphParams;
use crate::quant::Bit4Factors;

/// Runs the existing Vamana construction over a checked borrowed native row set.
pub(crate) enum NativeGraphBuildError<E> {
    Build(GraphBuildError),
    Control(E),
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
pub(crate) fn build_native_graph<E>(
    dimensions: usize,
    codes: &[u8],
    factors: &[Bit4Factors],
    rescore: &[f32],
    params: GraphParams,
    seed: u64,
    poll: &mut impl FnMut(u64) -> Result<(), E>,
    capacity: &mut impl FnMut(BuildMemoryEvent) -> Result<(), E>,
) -> Result<GraphBuildArtifact, NativeGraphBuildError<E>> {
    let vectors = SegmentVectors::from_native(dimensions, codes, factors, rescore)
        .map_err(NativeGraphBuildError::Build)?;
    let mut control_error = None;
    let mut memory_error = None;
    let result = {
        let mut bridge = |units| match poll(units) {
            Ok(()) => true,
            Err(error) => {
                control_error = Some(error);
                false
            }
        };
        let mut memory = |event| match capacity(event) {
            Ok(()) => true,
            Err(error) => {
                memory_error = Some(error);
                false
            }
        };
        let mut control = BuildControl {
            poll: Some(&mut bridge),
            memory: Some(&mut memory),
        };
        build_native_graph_inner(&vectors, params, seed, &mut control)
    };
    match (result, control_error, memory_error) {
        (_, Some(error), _) | (_, None, Some(error)) => Err(NativeGraphBuildError::Control(error)),
        (Ok(artifact), None, None) => Ok(artifact),
        (Err(error), None, None) => Err(NativeGraphBuildError::Build(error)),
    }
}

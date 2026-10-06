use super::{HEADER_BYTES, NATIVE_BUILD_SEED, put};
use crate::epoch::Normalization;
use crate::graph::GraphParams;
use crate::graph::block::decode_node_blocks_controlled;
use crate::graph::build::{
    BuildMemoryEvent, GraphBuildError, NativeGraphBuildError, build_native_graph,
};
use crate::lifecycle::StoreError;
use crate::property_graph::storage::artifact::BlockKind;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::payload::{PayloadRef, prepare_payload};
use crate::property_graph::storage::search::codec::encode_required;
use crate::property_graph::storage::tree::directory::{BlockSink, TreeError, TreeResources};
use crate::property_graph::wal::RequiredRef;
use crate::property_graph::{GraphGeneration, NodeId, StoreInstanceId};
use crate::quant::{Bit4ControlError, Bit4Scratch, quantize_bit4_controlled};

#[derive(Clone, Copy)]
pub(crate) struct NativeVectorRow {
    pub(crate) node: NodeId,
    pub(crate) revision: u64,
}

#[cfg(any(test, feature = "test-seams"))]
fn observe(
    stage: super::test_support::NativePrepareStage,
    units: u64,
    requested_bytes: usize,
    memory: &StorageMemory<'_>,
    resources: &TreeResources<'_>,
) {
    super::test_support::observe(super::test_support::NativePrepareEvent {
        stage,
        units,
        work_before: resources.work(),
        reserved_before: memory.reserved_bytes(),
        requested_bytes,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_vector_index<S: BlockSink>(
    sink: &mut S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    rows: &[NativeVectorRow],
    coordinates: &[f32],
    dimensions: usize,
    catalog: RequiredRef,
    normalization: Normalization,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    if rows.is_empty()
        || dimensions == 0
        || coordinates.len()
            != rows
                .len()
                .checked_mul(dimensions)
                .ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("native vector preparation geometry"));
    }
    let (profile, params) = match normalization {
        Normalization::None => (1_u8, GraphParams::sift_1m()),
        Normalization::L2 => {
            for row in coordinates.chunks_exact(dimensions) {
                if crate::graph::search::non_unit_squared_norm_controlled(row, &mut |units| {
                    resources.step(units)
                })?
                .is_some()
                {
                    return Err(TreeError::Invalid("native angular vector norm"));
                }
            }
            (2_u8, GraphParams::angular())
        }
    };
    let code_stride = dimensions.div_ceil(2);
    let code_length = rows
        .len()
        .checked_mul(code_stride)
        .ok_or(TreeError::Memory)?;
    let mut codes = StorageBuffer::new(memory, code_length)?;
    while codes.as_slice().len() < code_length {
        let count = (code_length - codes.as_slice().len()).min(256);
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::CodeInitialization,
            count as u64,
            0,
            memory,
            resources,
        );
        resources.step(count as u64)?;
        for _ in 0..count {
            codes.push(0)?;
        }
    }
    let mut factors = StorageBuffer::new(memory, rows.len())?;
    let scratch_bytes = Bit4Scratch::required_bytes(dimensions).ok_or(TreeError::Memory)?;
    let mut scratch_charge = memory.reserve(scratch_bytes)?;
    let mut scratch = Bit4Scratch::try_new(dimensions).map_err(|()| TreeError::Memory)?;
    scratch_charge.resize(scratch.owned_bytes().ok_or(TreeError::Memory)?)?;
    for (ordinal, row) in coordinates.chunks_exact(dimensions).enumerate() {
        let start = ordinal.checked_mul(code_stride).ok_or(TreeError::Memory)?;
        let end = start.checked_add(code_stride).ok_or(TreeError::Memory)?;
        let output = codes
            .as_mut_slice()
            .get_mut(start..end)
            .ok_or(TreeError::Invalid("native vector code output"))?;
        let factor = quantize_bit4_controlled(row, output, &mut scratch, &mut |units| {
            #[cfg(any(test, feature = "test-seams"))]
            observe(
                super::test_support::NativePrepareStage::Quantize,
                units,
                0,
                memory,
                resources,
            );
            resources.step(units)
        })
        .map_err(|error| match error {
            Bit4ControlError::Control(error) => error,
            Bit4ControlError::Memory => TreeError::Memory,
            Bit4ControlError::Quant(_) => TreeError::Invalid("native vector quantization"),
        })?;
        factors.push(factor)?;
    }
    let mut graph_charge = memory.reserve(0)?;
    let mut graph_live_bytes = 0_usize;
    let mut build = || {
        build_native_graph(
            dimensions,
            codes.as_slice(),
            factors.as_slice(),
            coordinates,
            params,
            NATIVE_BUILD_SEED,
            &mut |units| {
                #[cfg(any(test, feature = "test-seams"))]
                observe(
                    super::test_support::NativePrepareStage::Build,
                    units,
                    0,
                    memory,
                    resources,
                );
                resources.step(units)
            },
            &mut |event| {
                match event {
                    BuildMemoryEvent::Acquire(bytes) => {
                        graph_charge.resize(
                            graph_live_bytes
                                .checked_add(bytes)
                                .ok_or(TreeError::Memory)?,
                        )?;
                    }
                    BuildMemoryEvent::Reconcile { requested, actual } => {
                        if graph_charge.bytes()
                            != graph_live_bytes
                                .checked_add(requested)
                                .ok_or(TreeError::Memory)?
                        {
                            return Err(TreeError::Invalid("native graph memory acquisition"));
                        }
                        graph_live_bytes = graph_live_bytes
                            .checked_add(actual)
                            .ok_or(TreeError::Memory)?;
                        graph_charge.resize(graph_live_bytes)?;
                    }
                    BuildMemoryEvent::Release(bytes) => {
                        graph_live_bytes = graph_live_bytes
                            .checked_sub(bytes)
                            .ok_or(TreeError::Invalid("native graph memory release"))?;
                        graph_charge.resize(graph_live_bytes)?;
                    }
                }
                Ok(())
            },
        )
    };
    #[cfg(all(feature = "allocation-audit", any(test, feature = "test-seams")))]
    let graph = super::test_support::with_build_allocation_schedule(build);
    #[cfg(not(all(feature = "allocation-audit", any(test, feature = "test-seams"))))]
    let graph = build();
    let graph = graph.map_err(|error| match error {
        NativeGraphBuildError::Control(error) => error,
        NativeGraphBuildError::Build(GraphBuildError::Store(StoreError::AllocationFailed {
            ..
        })) => TreeError::Memory,
        NativeGraphBuildError::Build(_) => TreeError::Invalid("native vector graph build"),
    })?;
    if graph_live_bytes != graph.resident_bytes().map_err(|_| TreeError::Memory)? {
        return Err(TreeError::Invalid("native graph retained memory"));
    }
    let mut graph_control_error = None;
    let decoded_graph = decode_node_blocks_controlled(graph.encoded_region(), &mut |units| {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::OutputValidation,
            units,
            0,
            memory,
            resources,
        );
        match resources.step(units) {
            Ok(()) => true,
            Err(error) => {
                graph_control_error = Some(error);
                false
            }
        }
    });
    if let Some(error) = graph_control_error {
        return Err(error);
    }
    let decoded_graph =
        decoded_graph.map_err(|_| TreeError::Invalid("native vector graph output"))?;
    let rows_u32 = u32::try_from(rows.len()).map_err(|_| TreeError::Memory)?;
    let dimensions_u32 = u32::try_from(dimensions).map_err(|_| TreeError::Memory)?;
    let identities_length = rows.len().checked_mul(24).ok_or(TreeError::Memory)?;
    let factors_length = rows.len().checked_mul(12).ok_or(TreeError::Memory)?;
    let rescore_length = coordinates.len().checked_mul(4).ok_or(TreeError::Memory)?;
    let lengths = [
        identities_length,
        code_length,
        factors_length,
        rescore_length,
        graph.encoded_region().len(),
    ];
    let mut offsets = [0_usize; 5];
    let mut total = HEADER_BYTES;
    for (offset, length) in offsets.iter_mut().zip(lengths) {
        *offset = total;
        total = total.checked_add(length).ok_or(TreeError::Memory)?;
    }
    #[cfg(any(test, feature = "test-seams"))]
    observe(
        super::test_support::NativePrepareStage::ImageAllocation,
        0,
        total,
        memory,
        resources,
    );
    let mut image = StorageBuffer::new(memory, total)?;
    while image.as_slice().len() < total {
        let count = (total - image.as_slice().len()).min(256);
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::ImageInitialization,
            count as u64,
            0,
            memory,
            resources,
        );
        resources.step(count as u64)?;
        for _ in 0..count {
            image.push(0)?;
        }
    }
    let bytes = image.as_mut_slice();
    resources.step(HEADER_BYTES as u64)?;
    put(bytes, 0, b"ZGNVIDX1")?;
    put(bytes, 8, &1_u16.to_le_bytes())?;
    put(bytes, 10, &4_u16.to_le_bytes())?;
    put(bytes, 12, &1_u16.to_le_bytes())?;
    put(bytes, 14, &[profile, rows_u32.min(4) as u8])?;
    put(bytes, 16, &store.get().to_le_bytes())?;
    put(bytes, 32, &rows_u32.to_le_bytes())?;
    put(bytes, 36, &dimensions_u32.to_le_bytes())?;
    put(
        bytes,
        40,
        &decoded_graph.layout().padded_dims().to_le_bytes(),
    )?;
    put(bytes, 44, &[params.r_target(), params.r_max()])?;
    put(bytes, 46, &params.l_build().to_le_bytes())?;
    put(bytes, 48, &params.alpha_build().to_bits().to_le_bytes())?;
    put(bytes, 52, &params.alpha_refine().to_bits().to_le_bytes())?;
    put(bytes, 56, &NATIVE_BUILD_SEED.to_le_bytes())?;
    encode_required(
        catalog,
        bytes
            .get_mut(64..160)
            .ok_or(TreeError::Invalid("native vector catalog extent"))?,
    )?;
    for index in 0..5 {
        let base = 160 + index * 16;
        let offset = *offsets
            .get(index)
            .ok_or(TreeError::Invalid("native vector offset field"))?;
        let length = *lengths
            .get(index)
            .ok_or(TreeError::Invalid("native vector length field"))?;
        put(bytes, base, &(offset as u64).to_le_bytes())?;
        put(bytes, base + 8, &(length as u64).to_le_bytes())?;
    }
    let first_seed = graph
        .entry_points()
        .first()
        .copied()
        .ok_or(TreeError::Invalid("native vector graph seed"))?;
    for index in 0..4 {
        let seed = graph
            .entry_points()
            .get(index)
            .copied()
            .unwrap_or(first_seed);
        put(bytes, 240 + index * 4, &seed.to_le_bytes())?;
    }
    for (ordinal, row) in rows.iter().enumerate() {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::Serialization,
            24,
            0,
            memory,
            resources,
        );
        resources.step(24)?;
        let start = offsets
            .first()
            .copied()
            .ok_or(TreeError::Invalid("native vector identities offset"))?
            .checked_add(ordinal.checked_mul(24).ok_or(TreeError::Memory)?)
            .ok_or(TreeError::Memory)?;
        put(bytes, start, &row.node.get().to_le_bytes())?;
        put(bytes, start + 16, &row.revision.to_le_bytes())?;
    }
    let codes_offset = *offsets
        .get(1)
        .ok_or(TreeError::Invalid("native vector codes offset"))?;
    for (chunk_index, chunk) in codes.as_slice().chunks(256).enumerate() {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::Serialization,
            chunk.len() as u64,
            0,
            memory,
            resources,
        );
        resources.step(chunk.len() as u64)?;
        put(bytes, codes_offset + chunk_index * 256, chunk)?;
    }
    for (ordinal, factor) in factors.as_slice().iter().enumerate() {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::Serialization,
            12,
            0,
            memory,
            resources,
        );
        resources.step(12)?;
        let start = offsets
            .get(2)
            .copied()
            .ok_or(TreeError::Invalid("native vector factors offset"))?
            .checked_add(ordinal.checked_mul(12).ok_or(TreeError::Memory)?)
            .ok_or(TreeError::Memory)?;
        for (field, value) in factor.persisted_fields().iter().enumerate() {
            put(bytes, start + field * 4, &value.to_bits().to_le_bytes())?;
        }
    }
    let rescore_offset = offsets
        .get(3)
        .copied()
        .ok_or(TreeError::Invalid("native vector rescore offset"))?;
    for (chunk_index, chunk) in coordinates.chunks(256).enumerate() {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::Serialization,
            chunk.len() as u64,
            0,
            memory,
            resources,
        );
        resources.step(chunk.len() as u64)?;
        for (index, value) in chunk.iter().enumerate() {
            let ordinal = chunk_index
                .checked_mul(256)
                .and_then(|base| base.checked_add(index))
                .ok_or(TreeError::Memory)?;
            let start = rescore_offset
                .checked_add(ordinal.checked_mul(4).ok_or(TreeError::Memory)?)
                .ok_or(TreeError::Memory)?;
            put(bytes, start, &value.to_bits().to_le_bytes())?;
        }
    }
    let graph_offset = *offsets
        .get(4)
        .ok_or(TreeError::Invalid("native vector graph offset"))?;
    for (chunk_index, chunk) in graph.encoded_region().chunks(256).enumerate() {
        #[cfg(any(test, feature = "test-seams"))]
        observe(
            super::test_support::NativePrepareStage::Serialization,
            chunk.len() as u64,
            0,
            memory,
            resources,
        );
        resources.step(chunk.len() as u64)?;
        put(bytes, graph_offset + chunk_index * 256, chunk)?;
    }
    prepare_payload(
        sink,
        store,
        generation,
        BlockKind::RetrievalVectorIndex,
        image.as_slice(),
        resources,
    )
}

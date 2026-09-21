use super::{
    Bit4Factors, Bit4Query, CriticalValue, MAX_MAGNITUDE_LEVEL, QuantError, SplitMix64,
    apply_event, dot_bit4_prepared, normalized_score_squared, validate_code, validate_vector_shape,
};
use std::cmp::Ordering;

/// Fixed-capacity scratch shared by the public and controlled Bit4 entries.
pub(crate) struct Bit4Scratch {
    critical_values: Vec<CriticalValue>,
    magnitudes: Vec<u8>,
    dimensions: usize,
}

impl Bit4Scratch {
    pub(crate) fn required_bytes(dimensions: usize) -> Option<usize> {
        dimensions
            .checked_mul(MAX_MAGNITUDE_LEVEL as usize)
            .and_then(|events| events.checked_mul(std::mem::size_of::<CriticalValue>()))
            .and_then(|bytes| bytes.checked_add(dimensions))
    }

    pub(crate) fn try_new(dimensions: usize) -> Result<Self, ()> {
        let events = dimensions
            .checked_mul(MAX_MAGNITUDE_LEVEL as usize)
            .ok_or(())?;
        let mut critical_values = Vec::new();
        critical_values.try_reserve_exact(events).map_err(|_| ())?;
        let mut magnitudes = Vec::new();
        magnitudes.try_reserve_exact(dimensions).map_err(|_| ())?;
        magnitudes.resize(dimensions, 0);
        Ok(Self {
            critical_values,
            magnitudes,
            dimensions,
        })
    }

    pub(super) fn compatibility(dimensions: usize) -> Self {
        let events = dimensions.saturating_mul(MAX_MAGNITUDE_LEVEL as usize);
        Self {
            critical_values: Vec::with_capacity(events),
            magnitudes: vec![0; dimensions],
            dimensions,
        }
    }

    pub(crate) fn owned_bytes(&self) -> Option<usize> {
        self.critical_values
            .capacity()
            .checked_mul(std::mem::size_of::<CriticalValue>())
            .and_then(|bytes| bytes.checked_add(self.magnitudes.capacity()))
    }
}

pub(crate) enum Bit4ControlError<E> {
    Quant(QuantError),
    Memory,
    Control(E),
}

fn compare(left: &CriticalValue, right: &CriticalValue) -> Ordering {
    left.threshold
        .total_cmp(&right.threshold)
        .then_with(|| left.coordinate.cmp(&right.coordinate))
        .then_with(|| left.level.cmp(&right.level))
}

fn sift_down<E>(
    values: &mut [CriticalValue],
    mut root: usize,
    end: usize,
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), Bit4ControlError<E>> {
    loop {
        control(1).map_err(Bit4ControlError::Control)?;
        let Some(child) = root.checked_mul(2).and_then(|value| value.checked_add(1)) else {
            return Err(Bit4ControlError::Memory);
        };
        if child >= end {
            return Ok(());
        }
        let mut selected = child;
        if child + 1 < end {
            control(1).map_err(Bit4ControlError::Control)?;
            let left = values.get(child).ok_or(Bit4ControlError::Memory)?;
            let right = values.get(child + 1).ok_or(Bit4ControlError::Memory)?;
            if compare(left, right).is_lt() {
                selected = child + 1;
            }
        }
        control(1).map_err(Bit4ControlError::Control)?;
        let root_value = values.get(root).ok_or(Bit4ControlError::Memory)?;
        let selected_value = values.get(selected).ok_or(Bit4ControlError::Memory)?;
        if !compare(root_value, selected_value).is_lt() {
            return Ok(());
        }
        values.swap(root, selected);
        root = selected;
    }
}

fn controlled_sort<E>(
    values: &mut [CriticalValue],
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), Bit4ControlError<E>> {
    for root in (0..values.len() / 2).rev() {
        sift_down(values, root, values.len(), control)?;
    }
    for end in (1..values.len()).rev() {
        control(1).map_err(Bit4ControlError::Control)?;
        values.swap(0, end);
        sift_down(values, 0, end, control)?;
    }
    Ok(())
}

fn validate_vector_controlled<E>(
    values: &[f32],
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), Bit4ControlError<E>> {
    validate_vector_shape(values).map_err(Bit4ControlError::Quant)?;
    for (chunk_index, chunk) in values.chunks(256).enumerate() {
        control(chunk.len() as u64).map_err(Bit4ControlError::Control)?;
        if let Some((offset, _)) = chunk
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite())
        {
            return Err(Bit4ControlError::Quant(QuantError::NonFinite {
                index: chunk_index * 256 + offset,
            }));
        }
    }
    Ok(())
}

fn validate_input_controlled<E>(
    values: &[f32],
    output_len: usize,
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), Bit4ControlError<E>> {
    validate_vector_controlled(values, control)?;
    let expected = values.len().div_ceil(super::CODES_PER_BYTE);
    if output_len != expected {
        return Err(Bit4ControlError::Quant(QuantError::OutputLength {
            expected,
            actual: output_len,
        }));
    }
    Ok(())
}

pub(crate) fn quantize_bit4_controlled<E>(
    v: &[f32],
    out: &mut [u8],
    scratch: &mut Bit4Scratch,
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<Bit4Factors, Bit4ControlError<E>> {
    {
        #[cfg(all(feature = "graph-cypher", any(test, feature = "test-support")))]
        let _phase = crate::property_graph::storage::search::native_vector_validation_phase(
            crate::property_graph::storage::search::NativeVectorValidationStage::QuantizeValidation,
        );
        validate_input_controlled(v, out.len(), control)?;
    }
    if scratch.dimensions != v.len() {
        return Err(Bit4ControlError::Memory);
    }
    scratch.critical_values.clear();
    for chunk in scratch.magnitudes.chunks_mut(256) {
        control(chunk.len() as u64).map_err(Bit4ControlError::Control)?;
        chunk.fill(0);
    }

    let mut norm_squared = 0.0_f64;
    let mut absolute_sum = 0.0_f64;
    let mut row_scale = 0.0_f64;
    for (coordinate, &value) in v.iter().enumerate() {
        control(1).map_err(Bit4ControlError::Control)?;
        let value = f64::from(value);
        let magnitude = value.abs();
        norm_squared += value * value;
        absolute_sum += magnitude;
        row_scale = row_scale.max(magnitude);
        if magnitude > 0.0 {
            for level in 1..=MAX_MAGNITUDE_LEVEL {
                if scratch.critical_values.len() == scratch.critical_values.capacity() {
                    return Err(Bit4ControlError::Memory);
                }
                scratch.critical_values.push(CriticalValue {
                    threshold: f64::from(level) / magnitude,
                    coordinate,
                    level,
                    magnitude,
                });
            }
        }
    }
    controlled_sort(&mut scratch.critical_values, control)?;

    let mut numerator = 0.5 * absolute_sum;
    let mut grid_norm_squared = 0.25 * v.len() as f64;
    let mut best_numerator = numerator;
    let mut best_score_squared = normalized_score_squared(numerator, grid_norm_squared);
    let mut best_event_count = 0_usize;
    let mut event_count = 0_usize;
    let mut events = scratch.critical_values.iter().peekable();
    while let Some(event) = events.next() {
        control(1).map_err(Bit4ControlError::Control)?;
        let threshold = event.threshold;
        apply_event(event, &mut numerator, &mut grid_norm_squared);
        event_count += 1;
        while events
            .peek()
            .is_some_and(|next| next.threshold == threshold)
        {
            if let Some(tied) = events.next() {
                control(1).map_err(Bit4ControlError::Control)?;
                apply_event(tied, &mut numerator, &mut grid_norm_squared);
                event_count += 1;
            }
        }
        let score_squared = normalized_score_squared(numerator, grid_norm_squared);
        if score_squared > best_score_squared {
            best_score_squared = score_squared;
            best_numerator = numerator;
            best_event_count = event_count;
        }
    }
    for event in scratch.critical_values.iter().take(best_event_count) {
        control(1).map_err(Bit4ControlError::Control)?;
        let level = scratch
            .magnitudes
            .get_mut(event.coordinate)
            .ok_or(Bit4ControlError::Memory)?;
        *level = event.level;
    }
    for ((values, levels), byte) in v
        .chunks(2)
        .zip(scratch.magnitudes.chunks(2))
        .zip(out.iter_mut())
    {
        control(1).map_err(Bit4ControlError::Control)?;
        let mut packed = 0_u8;
        for (field, (&value, &level)) in values.iter().zip(levels).enumerate() {
            let unsigned = if value < 0.0 {
                MAX_MAGNITUDE_LEVEL - level
            } else {
                MAX_MAGNITUDE_LEVEL + 1 + level
            };
            let shift = 4_u32.saturating_sub((field as u32) * 4);
            packed |= unsigned << shift;
        }
        *byte = packed;
    }

    let (normalized_norm, normalized_correction) = if norm_squared == 0.0 {
        (0.0, 0.0)
    } else {
        (
            (norm_squared.sqrt() / row_scale) as f32,
            (norm_squared / (row_scale * best_numerator)) as f32,
        )
    };
    Ok(Bit4Factors {
        scale: row_scale as f32,
        normalized_norm,
        normalized_correction,
    })
}

pub(crate) fn est_dot_bit4_controlled<E>(
    query: &Bit4Query,
    codes: &[u8],
    factors: Bit4Factors,
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<f32, Bit4ControlError<E>> {
    validate_code(codes, query.codes.len()).map_err(Bit4ControlError::Quant)?;
    if factors.scale == 0.0 {
        return Ok(0.0);
    }
    let mut integer_dot = 0_i32;
    let mut coordinate = 0_usize;
    while coordinate < query.codes.len() {
        let remaining = query.codes.len() - coordinate;
        let dimensions = remaining.min(256);
        control(dimensions as u64).map_err(Bit4ControlError::Control)?;
        let query_end = coordinate
            .checked_add(dimensions)
            .ok_or(Bit4ControlError::Memory)?;
        let query_codes = query
            .codes
            .get(coordinate..query_end)
            .ok_or(Bit4ControlError::Memory)?;
        let code_start = coordinate / 2;
        let code_end = query_end.div_ceil(2);
        let row_codes = codes
            .get(code_start..code_end)
            .ok_or(Bit4ControlError::Memory)?;
        let code_sum = query_codes.iter().map(|value| i32::from(*value)).sum();
        integer_dot = integer_dot
            .checked_add(dot_bit4_prepared(query_codes, code_sum, row_codes))
            .ok_or(Bit4ControlError::Memory)?;
        coordinate = query_end;
    }
    Ok((factors.correction() * query.scale_half * f64::from(integer_dot)) as f32)
}

pub(crate) fn prepare_bit4_query_controlled<E>(
    q: &[f32],
    seed: u64,
    control: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(Bit4Query, usize), Bit4ControlError<E>> {
    {
        #[cfg(all(feature = "graph-cypher", any(test, feature = "test-support")))]
        let _phase = crate::property_graph::storage::search::native_vector_validation_phase(
            crate::property_graph::storage::search::NativeVectorValidationStage::QueryValidation,
        );
        validate_vector_controlled(q, control)?;
    }
    let mut max_absolute = 0.0_f32;
    for value in q {
        control(1).map_err(Bit4ControlError::Control)?;
        max_absolute = max_absolute.max(value.abs());
    }
    let mut coordinate_codes = Vec::new();
    #[cfg(feature = "allocation-audit")]
    let allocation =
        crate::allocation_audit::attributed(|| coordinate_codes.try_reserve_exact(q.len()));
    #[cfg(not(feature = "allocation-audit"))]
    let allocation = coordinate_codes.try_reserve_exact(q.len());
    allocation.map_err(|_| Bit4ControlError::Memory)?;
    let scale = if max_absolute == 0.0 {
        while coordinate_codes.len() < q.len() {
            let next = coordinate_codes
                .len()
                .checked_add((q.len() - coordinate_codes.len()).min(256))
                .ok_or(Bit4ControlError::Memory)?;
            control((next - coordinate_codes.len()) as u64).map_err(Bit4ControlError::Control)?;
            coordinate_codes.resize(next, 0);
        }
        0.0
    } else {
        let scale = f64::from(max_absolute) / 127.0;
        let mut random = SplitMix64::new(seed);
        for &value in q {
            control(1).map_err(Bit4ControlError::Control)?;
            let scaled = f64::from(value) / scale;
            let lower = scaled.floor();
            let probability_up = scaled - lower;
            let rounded = if random.next_open_unit_f64() < probability_up {
                lower + 1.0
            } else {
                lower
            };
            coordinate_codes.push(rounded.clamp(-127.0, 127.0) as i8);
        }
        scale
    };
    let mut code_sum = 0_i32;
    let mut codes = Vec::new();
    #[cfg(feature = "allocation-audit")]
    let allocation = crate::allocation_audit::attributed(|| codes.try_reserve_exact(q.len()));
    #[cfg(not(feature = "allocation-audit"))]
    let allocation = codes.try_reserve_exact(q.len());
    allocation.map_err(|_| Bit4ControlError::Memory)?;
    for block in coordinate_codes.chunks(32) {
        for code in block.iter().step_by(2) {
            control(1).map_err(Bit4ControlError::Control)?;
            code_sum += i32::from(*code);
            codes.push(*code);
        }
        for code in block.iter().skip(1).step_by(2) {
            control(1).map_err(Bit4ControlError::Control)?;
            code_sum += i32::from(*code);
            codes.push(*code);
        }
    }
    let peak_bytes = coordinate_codes
        .capacity()
        .checked_add(codes.capacity())
        .ok_or(Bit4ControlError::Memory)?;
    Ok((
        Bit4Query {
            codes,
            code_sum,
            scale_half: scale * 0.5,
        },
        peak_bytes,
    ))
}

//! Single-pass identity codec transport with bounded, actually charged scratch.
use super::*;
use crate::property_graph::OperationProvenance;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use std::io::{self, Write};

/// Persist the existing identity-owned provenance codec exactly once. No complete
/// key/provenance image is copied; writer scratch and descriptor share the actual
/// storage/writer/store allowance. Errors retain any private sink abort inventory.
pub fn prepare_provenance<S: BlockSink>(
    sink: &mut S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    provenance: OperationProvenance<'_>,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    r.require_preparation(memory)?;
    let length = usize::try_from(provenance.encoded_len()).map_err(|_| TreeError::Memory)?;
    validate_prepared(BlockKind::OperationProvenance, length)?;
    let mut writer = StreamWriter::new(sink, store, generation, length, memory, r)?;
    let encoded = provenance.write_to(&mut writer, &mut || Ok(()));
    if let Some(error) = writer.failure.take() {
        return Err(error);
    }
    let stats = encoded.map_err(|_| TreeError::Invalid("provenance encoding failed"))?;
    if stats.bytes != provenance.encoded_len() {
        return Err(TreeError::Invalid("provenance encoded length changed"));
    }
    writer.finish()
}
struct StreamWriter<'s, 'm, 'c, S: BlockSink> {
    sink: &'s mut S,
    r: &'s mut TreeResources<'c>,
    store: StoreInstanceId,
    generation: GraphGeneration,
    length: usize,
    seen: usize,
    used: usize,
    emitted: usize,
    buffer: StorageBuffer<'m, u8>,
    descriptor: StorageBuffer<'m, u8>,
    failure: Option<TreeError>,
}
impl<'s, 'm, 'c, S: BlockSink> StreamWriter<'s, 'm, 'c, S> {
    fn new(
        sink: &'s mut S,
        store: StoreInstanceId,
        generation: GraphGeneration,
        length: usize,
        memory: &'m StorageMemory<'m>,
        r: &'s mut TreeResources<'c>,
    ) -> Result<Self, TreeError> {
        let buffer = zeros(memory, length.min(CHUNK_BYTES), r)?;
        let descriptor = if length > CHUNK_BYTES {
            let count = length.div_ceil(CHUNK_BYTES);
            let mut bytes = zeros(memory, 32 + count * 32, r)?;
            let output = bytes.as_mut_slice();
            r.step(32)?;
            put(output, 0, b"ZGEX")?;
            put(output, 4, &1u16.to_le_bytes())?;
            put(
                output,
                6,
                &(BlockKind::OperationProvenance as u16).to_le_bytes(),
            )?;
            put(output, 8, &(length as u64).to_le_bytes())?;
            put(output, 16, &(count as u32).to_le_bytes())?;
            put(output, 20, &(CHUNK_BYTES as u32).to_le_bytes())?;
            bytes
        } else {
            StorageBuffer::new(memory, 0)?
        };
        Ok(Self {
            sink,
            r,
            store,
            generation,
            length,
            seen: 0,
            used: 0,
            emitted: 0,
            buffer,
            descriptor,
            failure: None,
        })
    }
    fn write_inner(&mut self, bytes: &[u8]) -> Result<(), TreeError> {
        if self
            .seen
            .checked_add(bytes.len())
            .is_none_or(|end| end > self.length)
        {
            return Err(TreeError::Invalid("provenance exceeded preflight length"));
        }
        let mut position = 0;
        while position < bytes.len() {
            let count = (self.buffer.as_slice().len() - self.used).min(bytes.len() - position);
            if count == 0 {
                return Err(TreeError::Invalid("zero provenance write capacity"));
            }
            self.r.step(count as u64)?;
            self.buffer
                .as_mut_slice()
                .get_mut(self.used..self.used + count)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(
                    bytes
                        .get(position..position + count)
                        .ok_or(TreeError::Memory)?,
                );
            position += count;
            self.used += count;
            self.seen += count;
            if self.used == CHUNK_BYTES && self.length > CHUNK_BYTES {
                self.flush_chunk()?;
            }
        }
        self.r.step(0)?;
        Ok(())
    }
    fn flush_chunk(&mut self) -> Result<(), TreeError> {
        self.r.step(0)?;
        let bytes = self
            .buffer
            .as_slice()
            .get(..self.used)
            .ok_or(TreeError::Memory)?;
        let reference = append_verified(
            self.sink,
            self.store,
            self.generation,
            BlockKind::PayloadChunk,
            bytes,
            self.r,
        )?;
        let start = self
            .emitted
            .checked_mul(32)
            .and_then(|n| n.checked_add(32))
            .ok_or(TreeError::Memory)?;
        self.r.step(32)?;
        artifact::encode_reference(
            reference,
            self.descriptor
                .as_mut_slice()
                .get_mut(start..start + 32)
                .ok_or(TreeError::Invalid("provenance extent count"))?,
        )?;
        self.emitted += 1;
        self.used = 0;
        self.r.step(0)?;
        Ok(())
    }
    fn finish(mut self) -> Result<PayloadRef, TreeError> {
        self.r.step(0)?;
        if self.seen != self.length {
            return Err(TreeError::Invalid("short provenance stream"));
        }
        let reference = if self.length <= CHUNK_BYTES {
            append_verified(
                self.sink,
                self.store,
                self.generation,
                BlockKind::OperationProvenance,
                self.buffer
                    .as_slice()
                    .get(..self.used)
                    .ok_or(TreeError::Memory)?,
                self.r,
            )?
        } else {
            if self.used != 0 {
                self.flush_chunk()?;
            }
            if self.emitted != self.length.div_ceil(CHUNK_BYTES) {
                return Err(TreeError::Invalid("provenance extent completeness"));
            }
            append_verified(
                self.sink,
                self.store,
                self.generation,
                BlockKind::ExtentList,
                self.descriptor.as_slice(),
                self.r,
            )?
        };
        self.r.step(0)?;
        PayloadRef::new(
            BlockKind::OperationProvenance,
            self.length as u64,
            reference,
        )
    }
}
impl<S: BlockSink> Write for StreamWriter<'_, '_, '_, S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() {
            return Err(io::ErrorKind::Other.into());
        }
        match self.write_inner(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.failure = Some(error);
                Err(io::ErrorKind::Other.into())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.failure.is_some() {
            return Err(io::ErrorKind::Other.into());
        }
        match self.r.step(0) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failure = Some(error);
                Err(io::ErrorKind::Other.into())
            }
        }
    }
}
fn zeros<'a>(
    memory: &'a StorageMemory<'a>,
    count: usize,
    r: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'a, u8>, TreeError> {
    let mut buffer = StorageBuffer::new(memory, count)?;
    let zeros = [0; 1024];
    while buffer.as_slice().len() < count {
        let length = (count - buffer.as_slice().len()).min(zeros.len());
        r.step(length as u64)?;
        buffer.extend_from_slice(zeros.get(..length).ok_or(TreeError::Memory)?)?;
    }
    Ok(buffer)
}

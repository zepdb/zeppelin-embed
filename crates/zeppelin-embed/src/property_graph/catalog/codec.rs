use super::work;
use super::{
    CatalogDeclaration, CatalogError, DocumentDeclaration, GraphInterpretation, Symbol,
    SymbolCatalog, SymbolEntry, SymbolHighWaters, SymbolKind,
};
use crate::epoch::{ComputeUnits, EmbeddingRuntime, Normalization};
use crate::fts::tokenizer::TokenizerEpoch;
use crate::property_graph::{GraphName, MAX_GRAPH_INPUT_BYTES, StoreInstanceId};
use xxhash_rust::xxh3::Xxh3;

const PREFIX: usize = 120;
const CHUNK: usize = 64 * 1024;
type Checkpoint<'a> = &'a mut dyn FnMut() -> Result<(), CatalogError>;

/// A reconstructed logical catalog participant, borrowing immutable input bytes.
/// This does not validate a whole-store checkpoint or authorize recovery/cleanup.
#[derive(Debug)]
pub struct CatalogImage<'a> {
    /// Interpretation and allocator metadata, with the original store identity.
    pub declaration: CatalogDeclaration<'a>,
    /// Bounded working descriptors; all name backing remains borrowed.
    pub symbols: SymbolCatalog<'a>,
}
impl<'a> CatalogImage<'a> {
    /// Returns the complete logical byte reservation, including the checksum.
    pub fn encoded_len(&self, checkpoint: Checkpoint<'_>) -> Result<usize, CatalogError> {
        checkpoint()?;
        let mut bytes = PREFIX + 8;
        if let Some(doc) = self.declaration.interpretation.embedding {
            doc.validate()?;
            bytes = add(bytes, 47)?;
            for len in [
                doc.model_id.len(),
                doc.model_version.len(),
                doc.weights_digest.len(),
                doc.prompt_prefix.len(),
            ] {
                bytes = add(bytes, len)?;
            }
            if let Some(os) = doc.os_build {
                bytes = add(bytes, add(8, os.len())?)?;
            }
        }
        for entry in self.symbols.entries() {
            checkpoint()?;
            bytes = add(bytes, add(24, entry.name.as_str().len())?)?;
        }
        Ok(bytes)
    }
    /// Encodes into caller-reserved storage. Capacity refusal changes no bytes;
    /// cancellation may leave a private prefix and never constitutes publication.
    pub fn encode_into(
        &self,
        output: &mut [u8],
        checkpoint: Checkpoint<'_>,
    ) -> Result<usize, CatalogError> {
        checkpoint()?;
        let length = self.encoded_len(checkpoint)?;
        let target = output.get_mut(..length).ok_or(CatalogError::Capacity)?;
        let mut writer = Writer {
            bytes: target,
            offset: 0,
            checkpoint,
        };
        writer.emit(b"ZGCA")?;
        writer.emit(&1_u16.to_le_bytes())?;
        writer.emit(&1_u16.to_le_bytes())?;
        writer.emit(&(length as u64).to_le_bytes())?;
        writer.emit(&self.declaration.store.get().to_le_bytes())?;
        writer.emit(&self.declaration.node_high_water.to_le_bytes())?;
        writer.emit(&self.declaration.relationship_high_water.to_le_bytes())?;
        let waters = self.symbols.high_waters();
        for value in [
            waters.label,
            waters.relationship_type,
            waters.property,
            waters.namespace,
            self.declaration.interpretation.lexical.value(),
            self.symbols.entries.len() as u64,
        ] {
            writer.emit(&value.to_le_bytes())?;
        }
        let embedding = self.declaration.interpretation.embedding;
        writer.emit(&[u8::from(embedding.is_some())])?;
        writer.emit(&[0; 7])?;
        if let Some(doc) = embedding {
            writer.blob(doc.model_id.as_bytes())?;
            writer.blob(doc.model_version.as_bytes())?;
            writer.blob(doc.weights_digest)?;
            writer.emit(&doc.dims.to_le_bytes())?;
            writer.emit(&(doc.normalization as u16).to_le_bytes())?;
            writer.blob(doc.prompt_prefix.as_bytes())?;
            writer.emit(&doc.max_tokens.to_le_bytes())?;
            writer.emit(&(doc.runtime as u16).to_le_bytes())?;
            writer.emit(&(doc.compute_units as u16).to_le_bytes())?;
            writer.emit(&[u8::from(doc.os_build.is_some())])?;
            if let Some(os) = doc.os_build {
                writer.blob(os.as_bytes())?;
            }
        }
        for entry in self.symbols.entries() {
            writer.emit(&[entry.symbol.kind() as u8])?;
            writer.emit(&[0; 7])?;
            writer.emit(&entry.symbol.get().to_le_bytes())?;
            writer.blob(entry.name.as_str().as_bytes())?;
        }
        let data = writer
            .bytes
            .get(..writer.offset)
            .ok_or(CatalogError::Malformed)?;
        let checksum = digest(data, writer.checkpoint)?;
        writer.emit(&checksum.to_le_bytes())?;
        Ok(writer.offset)
    }
    /// Reconstructs declarations and borrowed names after complete validation.
    /// The descriptor allowance is reserved from the shared budget by the caller.
    pub fn decode(
        bytes: &'a [u8],
        descriptor_allowance: usize,
        checkpoint: Checkpoint<'_>,
    ) -> Result<Self, CatalogError> {
        checkpoint()?;
        if bytes.len() < PREFIX + 8 {
            return Err(CatalogError::Malformed);
        }
        let mut reader = Reader { bytes, offset: 0 };
        if reader.take(4)? != b"ZGCA" {
            return Err(CatalogError::Malformed);
        }
        if reader.u16()? != 1 || reader.u16()? != 1 {
            return Err(CatalogError::Unsupported);
        }
        if reader.u64()? != bytes.len() as u64 {
            return Err(CatalogError::Malformed);
        }
        let store = StoreInstanceId::new(reader.u128()?).map_err(|_| CatalogError::Malformed)?;
        let node_high_water = reader.u128()?;
        let relationship_high_water = reader.u128()?;
        let waters = SymbolHighWaters {
            label: reader.u64()?,
            relationship_type: reader.u64()?,
            property: reader.u64()?,
            namespace: reader.u64()?,
        };
        let lexical = TokenizerEpoch::from_value(reader.u64()?);
        let count = usize::try_from(reader.u64()?).map_err(|_| CatalogError::Capacity)?;
        if count > (bytes.len() - PREFIX - 8) / 24 {
            return Err(CatalogError::Malformed);
        }
        let embedding_present = reader.tag()?;
        reader.zero(7)?;
        let trailer = bytes.len() - 8;
        let expected = u64::from_le_bytes(
            bytes
                .get(trailer..)
                .and_then(|v| v.first_chunk::<8>())
                .copied()
                .ok_or(CatalogError::Malformed)?,
        );
        if digest(
            bytes.get(..trailer).ok_or(CatalogError::Malformed)?,
            checkpoint,
        )? != expected
        {
            return Err(CatalogError::Checksum);
        }
        let embedding = if embedding_present {
            let value = DocumentDeclaration {
                model_id: reader.text(checkpoint)?,
                model_version: reader.text(checkpoint)?,
                weights_digest: reader.blob()?,
                dims: reader.u32()?,
                normalization: match reader.u16()? {
                    0 => Normalization::None,
                    1 => Normalization::L2,
                    _ => return Err(CatalogError::Unsupported),
                },
                prompt_prefix: reader.text(checkpoint)?,
                max_tokens: reader.u32()?,
                runtime: match reader.u16()? {
                    1 => EmbeddingRuntime::CoreMl,
                    2 => EmbeddingRuntime::Mlx,
                    3 => EmbeddingRuntime::CpuReference,
                    _ => return Err(CatalogError::Unsupported),
                },
                compute_units: match reader.u16()? {
                    1 => ComputeUnits::Cpu,
                    2 => ComputeUnits::CpuAndGpu,
                    3 => ComputeUnits::CpuAndNeuralEngine,
                    4 => ComputeUnits::All,
                    _ => return Err(CatalogError::Unsupported),
                },
                os_build: if reader.tag()? {
                    Some(reader.text(checkpoint)?)
                } else {
                    None
                },
            };
            value.validate()?;
            Some(value)
        } else {
            None
        };
        let declaration = CatalogDeclaration {
            store,
            node_high_water,
            relationship_high_water,
            interpretation: GraphInterpretation { lexical, embedding },
        };
        let mut symbols =
            SymbolCatalog::reconstruct(&[], waters, count, descriptor_allowance, checkpoint)?;
        for _ in 0..count {
            checkpoint()?;
            let kind = match reader.u8()? {
                1 => SymbolKind::Label,
                2 => SymbolKind::RelationshipType,
                3 => SymbolKind::Property,
                4 => SymbolKind::Namespace,
                _ => return Err(CatalogError::Unsupported),
            };
            reader.zero(7)?;
            let symbol = Symbol::new(kind, reader.u64()?)?;
            let name =
                GraphName::new(reader.text(checkpoint)?).map_err(|_| CatalogError::Malformed)?;
            symbols.entries.push(SymbolEntry { symbol, name });
        }
        if reader.offset != trailer {
            return Err(CatalogError::Malformed);
        }
        symbols.validate(checkpoint)?;
        Ok(Self {
            declaration,
            symbols,
        })
    }
}

fn add(left: usize, right: usize) -> Result<usize, CatalogError> {
    left.checked_add(right).ok_or(CatalogError::Capacity)
}
fn digest(bytes: &[u8], checkpoint: Checkpoint<'_>) -> Result<u64, CatalogError> {
    let mut hash = Xxh3::new();
    for chunk in bytes.chunks(CHUNK) {
        checkpoint()?;
        hash.update(chunk);
    }
    Ok(hash.digest())
}
struct Writer<'a, 'c> {
    bytes: &'a mut [u8],
    offset: usize,
    checkpoint: Checkpoint<'c>,
}
impl Writer<'_, '_> {
    fn emit(&mut self, bytes: &[u8]) -> Result<(), CatalogError> {
        for chunk in bytes.chunks(CHUNK) {
            (self.checkpoint)()?;
            let end = add(self.offset, chunk.len())?;
            self.bytes
                .get_mut(self.offset..end)
                .ok_or(CatalogError::Capacity)?
                .copy_from_slice(chunk);
            self.offset = end;
        }
        Ok(())
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<(), CatalogError> {
        self.emit(&(bytes.len() as u64).to_le_bytes())?;
        self.emit(bytes)
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], CatalogError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(CatalogError::Malformed)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CatalogError::Malformed)?;
        self.offset = end;
        Ok(value)
    }
    fn word<const N: usize>(&mut self) -> Result<[u8; N], CatalogError> {
        self.take(N)?
            .try_into()
            .map_err(|_| CatalogError::Malformed)
    }
    fn u8(&mut self) -> Result<u8, CatalogError> {
        Ok(u8::from_le_bytes(self.word()?))
    }
    fn u16(&mut self) -> Result<u16, CatalogError> {
        Ok(u16::from_le_bytes(self.word()?))
    }
    fn u32(&mut self) -> Result<u32, CatalogError> {
        Ok(u32::from_le_bytes(self.word()?))
    }
    fn u64(&mut self) -> Result<u64, CatalogError> {
        Ok(u64::from_le_bytes(self.word()?))
    }
    fn u128(&mut self) -> Result<u128, CatalogError> {
        Ok(u128::from_le_bytes(self.word()?))
    }
    fn zero(&mut self, length: usize) -> Result<(), CatalogError> {
        if self.take(length)?.iter().any(|v| *v != 0) {
            Err(CatalogError::Malformed)
        } else {
            Ok(())
        }
    }
    fn tag(&mut self) -> Result<bool, CatalogError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CatalogError::Unsupported),
        }
    }
    fn blob(&mut self) -> Result<&'a [u8], CatalogError> {
        let length = usize::try_from(self.u64()?).map_err(|_| CatalogError::Malformed)?;
        if length > MAX_GRAPH_INPUT_BYTES {
            return Err(CatalogError::Malformed);
        }
        self.take(length)
    }
    fn text(&mut self, checkpoint: Checkpoint<'_>) -> Result<&'a str, CatalogError> {
        work::utf8(self.blob()?, checkpoint)
    }
}

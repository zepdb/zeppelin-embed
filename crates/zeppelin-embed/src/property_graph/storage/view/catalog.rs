//! Exact read-only catalog bound to one admitted native bundle.

use super::source::NativeQuerySource;
use crate::property_graph::catalog::{
    CatalogError, CatalogImage, GraphInterpretation, Symbol, SymbolEntry, SymbolHighWaters,
    SymbolKind,
};
use crate::property_graph::query::resources::{QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::RuntimeError;
use crate::property_graph::query::runtime::RuntimeInstanceId;
use crate::property_graph::storage::artifact::BlockKind;
use crate::property_graph::storage::records::RecordCatalog;
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};

pub(crate) struct NativeCatalog<'a, 'm, 'g> {
    image: CatalogImage<'a>,
    lease_token: u64,
    runtime: RuntimeInstanceId,
    memory: &'m QueryMemory<'g>,
    _descriptors: QueryReservation<'m, 'g>,
}

impl<'a, 'm, 'g> NativeCatalog<'a, 'm, 'g> {
    pub(crate) fn open(
        source: &'a NativeQuerySource<'_, 'm, 'g>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        let bundle = source.lease().bundle();
        let memory = source.memory();
        let required = bundle.catalog();
        let block = source.resolve(required.block, resources)?;
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.reference() != required.block
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || block.reference().kind != BlockKind::CommitParticipant
        {
            return Err(TreeError::Invalid("catalog required descriptor mismatch"));
        }
        let payload = block.payload();
        let header = payload
            .get(..8)
            .ok_or(TreeError::Invalid("catalog participant header"))?;
        if header.get(..4) != Some(b"ZGCP".as_slice())
            || u16::from_le_bytes(
                *header
                    .get(4..6)
                    .and_then(|bytes| bytes.first_chunk::<2>())
                    .ok_or(TreeError::Invalid("catalog participant role"))?,
            ) != 1
            || u16::from_le_bytes(
                *header
                    .get(6..8)
                    .and_then(|bytes| bytes.first_chunk::<2>())
                    .ok_or(TreeError::Invalid("catalog participant version"))?,
            ) != 1
        {
            return Err(TreeError::Invalid("catalog participant role or version"));
        }
        let encoded = payload
            .get(8..)
            .ok_or(TreeError::Invalid("catalog participant payload"))?;
        let count = u64::from_le_bytes(
            *encoded
                .get(104..112)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .ok_or(TreeError::Invalid("catalog symbol count"))?,
        );
        let count = usize::try_from(count).map_err(|_| TreeError::Memory)?;
        let allowance = count
            .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
            .ok_or(TreeError::Memory)?;
        let mut descriptors = memory
            .reserve(allowance)
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let image = CatalogImage::decode(encoded, allowance, &mut || {
            resources.step(1).map_err(|_| CatalogError::Cancelled)
        })
        .map_err(|error| catalog_error(error, resources))?;
        descriptors
            .resize(image.symbols.allocated_bytes())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;

        let expected = GraphInterpretation::new(bundle.lexical(), bundle.document())
            .map_err(|_| TreeError::Invalid("invalid admitted catalog interpretation"))?;
        image
            .declaration
            .validate_for(bundle.base().store, expected, &mut || {
                resources.step(1).map_err(|_| CatalogError::Cancelled)
            })
            .map_err(|error| catalog_error(error, resources))?;
        let high = bundle.high_waters();
        if image.declaration.node_high_water != high.node
            || image.declaration.relationship_high_water != high.relationship
            || image.symbols.high_waters()
                != (SymbolHighWaters {
                    label: high.symbols[0],
                    relationship_type: high.symbols[1],
                    property: high.symbols[2],
                    namespace: high.symbols[3],
                })
        {
            return Err(TreeError::Invalid("catalog allocator high-water mismatch"));
        }
        Ok(Self {
            image,
            lease_token: source.lease().token(),
            runtime: source.runtime(),
            memory,
            _descriptors: descriptors,
        })
    }

    pub(super) fn owns(&self, source: &NativeQuerySource<'_, 'm, 'g>) -> bool {
        self.lease_token == source.lease().token()
            && self.runtime == source.runtime()
            && std::ptr::eq(self.memory, source.memory())
    }
}

fn catalog_error(error: CatalogError, resources: &mut TreeResources<'_>) -> TreeError {
    if error == CatalogError::Cancelled {
        resources
            .step(0)
            .err()
            .unwrap_or(TreeError::Invalid("catalog cancelled"))
    } else {
        TreeError::Invalid("invalid native graph catalog")
    }
}

impl<S: BlockSource> RecordCatalog<S> for NativeCatalog<'_, '_, '_> {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        name.validate_utf8(resources)?;
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol.kind() == kind
                && name.compare_bytes(entry.name.as_str().as_bytes(), resources)?
                    == std::cmp::Ordering::Equal
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid(
            "record name is absent from admitted catalog",
        ))
    }
}

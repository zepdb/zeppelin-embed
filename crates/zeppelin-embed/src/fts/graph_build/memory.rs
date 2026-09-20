use super::{GraphLexicalError, map_query_memory};
use crate::fts::control::{BuildPolicy, CapacityCharge};
use crate::property_graph::query::resources::{QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError, WorkKind};
use crate::property_graph::storage::memory::{StorageMemory, StorageReservation};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};

pub(super) enum Owner<'m> {
    Preparation(&'m StorageMemory<'m>),
    Query(&'m QueryMemory<'m>),
}

pub(super) enum Charge<'m> {
    Preparation(StorageReservation<'m>),
    Query(QueryReservation<'m, 'm>),
}

impl CapacityCharge for Charge<'_> {
    fn bytes(&self) -> usize {
        match self {
            Self::Preparation(charge) => charge.bytes(),
            Self::Query(charge) => charge.bytes(),
        }
    }
}

impl<'m> Owner<'m> {
    pub(super) fn reserve(&self, bytes: usize) -> Result<Charge<'m>, GraphLexicalError> {
        match self {
            Self::Preparation(memory) => memory
                .reserve(bytes)
                .map(Charge::Preparation)
                .map_err(GraphLexicalError::Resource),
            Self::Query(memory) => memory
                .reserve(bytes)
                .map(Charge::Query)
                .map_err(map_query_memory),
        }
    }
}

pub(super) struct PreparationPolicy<'r, 'q, 'm> {
    pub(super) owner: Owner<'m>,
    pub(super) resources: &'r mut TreeResources<'q>,
}

impl<'r, 'q, 'm> PreparationPolicy<'r, 'q, 'm> {
    pub(super) fn new(
        memory: &'m StorageMemory<'m>,
        resources: &'r mut TreeResources<'q>,
    ) -> Result<Self, GraphLexicalError> {
        resources
            .require_preparation(memory)
            .map_err(GraphLexicalError::Resource)?;
        resources.step(0).map_err(GraphLexicalError::Resource)?;
        Ok(Self {
            owner: Owner::Preparation(memory),
            resources,
        })
    }
}

impl<'r, 'q, 'm> BuildPolicy<'m> for PreparationPolicy<'r, 'q, 'm> {
    type Error = GraphLexicalError;
    type Charge = Charge<'m>;

    fn checkpoint(&mut self) -> Result<(), Self::Error> {
        self.resources.step(0).map_err(GraphLexicalError::Resource)
    }

    fn step(&mut self, units: u64) -> Result<(), Self::Error> {
        self.resources
            .step(units)
            .map_err(GraphLexicalError::Resource)?;
        #[cfg(test)]
        if units > 0 {
            crate::fts::control::test_stage_probe(crate::fts::control::TestStage::Work);
        }
        Ok(())
    }

    fn copy_step(&mut self, bytes: usize) -> Result<(), Self::Error> {
        self.step(u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    fn allocate_vec<T>(&mut self, capacity: usize) -> Result<(Vec<T>, Self::Charge), Self::Error> {
        self.checkpoint()?;
        let requested = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(GraphLexicalError::Resource(TreeError::Memory))?;
        let mut charge = self.owner.reserve(requested)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = values.try_reserve_exact(capacity);
        allocation.map_err(|_| GraphLexicalError::Resource(TreeError::Memory))?;
        let actual = values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(GraphLexicalError::Resource(TreeError::Memory))?;
        match &mut charge {
            Charge::Preparation(charge) => {
                charge.resize(actual).map_err(GraphLexicalError::Resource)?
            }
            Charge::Query(_) => {
                return Err(GraphLexicalError::Resource(TreeError::Invalid(
                    "lexical preparation charge owner mismatch",
                )));
            }
        }
        Ok((values, charge))
    }

    fn allocate_string(&mut self, capacity: usize) -> Result<(String, Self::Charge), Self::Error> {
        self.checkpoint()?;
        let mut charge = self.owner.reserve(capacity)?;
        let mut value = String::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| value.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = value.try_reserve_exact(capacity);
        allocation.map_err(|_| GraphLexicalError::Resource(TreeError::Memory))?;
        match &mut charge {
            Charge::Preparation(charge) => charge
                .resize(value.capacity())
                .map_err(GraphLexicalError::Resource)?,
            Charge::Query(_) => {
                return Err(GraphLexicalError::Resource(TreeError::Invalid(
                    "lexical preparation charge owner mismatch",
                )));
            }
        }
        Ok((value, charge))
    }
}

pub(super) struct QueryPolicy<'r, 'v, 'q, 'g, 'm> {
    pub(super) owner: Owner<'m>,
    pub(super) context: &'r mut RuntimeContext<'v, 'q, 'g>,
}

impl<'r, 'v, 'q, 'g, 'm> QueryPolicy<'r, 'v, 'q, 'g, 'm> {
    pub(super) fn new(
        memory: &'m QueryMemory<'m>,
        context: &'r mut RuntimeContext<'v, 'q, 'g>,
    ) -> Result<Self, GraphLexicalError> {
        context.checkpoint().map_err(map_runtime)?;
        if !std::ptr::eq(context.memory(), memory) {
            return Err(GraphLexicalError::Resource(TreeError::Invalid(
                "lexical query owner mismatch",
            )));
        }
        Ok(Self {
            owner: Owner::Query(memory),
            context,
        })
    }
}

impl<'r, 'v, 'q, 'g, 'm> BuildPolicy<'m> for QueryPolicy<'r, 'v, 'q, 'g, 'm> {
    type Error = GraphLexicalError;
    type Charge = Charge<'m>;

    fn checkpoint(&mut self) -> Result<(), Self::Error> {
        self.context.checkpoint().map_err(map_runtime)
    }

    fn step(&mut self, units: u64) -> Result<(), Self::Error> {
        let _ = units;
        self.context.checkpoint().map_err(map_runtime)
    }

    fn copy_step(&mut self, bytes: usize) -> Result<(), Self::Error> {
        let units = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.context
            .check_work(WorkKind::CopiedBytes, units)
            .map_err(map_runtime)?;
        self.context
            .charge(WorkKind::CopiedBytes, units)
            .map_err(map_runtime)
    }

    fn lexical_block(&mut self) -> Result<(), Self::Error> {
        self.context
            .charge(WorkKind::LexicalBlocks, 1)
            .map_err(map_runtime)
    }

    fn lexical_posting(&mut self) -> Result<(), Self::Error> {
        self.context
            .charge(WorkKind::LexicalPostings, 1)
            .map_err(map_runtime)
    }

    fn allocate_vec<T>(&mut self, capacity: usize) -> Result<(Vec<T>, Self::Charge), Self::Error> {
        self.checkpoint()?;
        let requested = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| {
                map_query_memory(crate::property_graph::query::resources::MemoryError::Limit)
            })?;
        let mut charge = self.owner.reserve(requested)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = values.try_reserve_exact(capacity);
        allocation.map_err(|_| {
            map_query_memory(crate::property_graph::query::resources::MemoryError::Allocation)
        })?;
        let actual = values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| {
                map_query_memory(crate::property_graph::query::resources::MemoryError::Limit)
            })?;
        match &mut charge {
            Charge::Query(charge) => charge.resize(actual).map_err(map_query_memory)?,
            Charge::Preparation(_) => {
                return Err(GraphLexicalError::Resource(TreeError::Invalid(
                    "lexical query charge owner mismatch",
                )));
            }
        }
        Ok((values, charge))
    }

    fn allocate_string(&mut self, capacity: usize) -> Result<(String, Self::Charge), Self::Error> {
        self.checkpoint()?;
        let mut charge = self.owner.reserve(capacity)?;
        let mut value = String::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| value.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = value.try_reserve_exact(capacity);
        allocation.map_err(|_| {
            map_query_memory(crate::property_graph::query::resources::MemoryError::Allocation)
        })?;
        match &mut charge {
            Charge::Query(charge) => charge.resize(value.capacity()).map_err(map_query_memory)?,
            Charge::Preparation(_) => {
                return Err(GraphLexicalError::Resource(TreeError::Invalid(
                    "lexical query charge owner mismatch",
                )));
            }
        }
        Ok((value, charge))
    }
}

fn map_runtime(error: RuntimeError) -> GraphLexicalError {
    GraphLexicalError::Resource(TreeError::Runtime(error))
}

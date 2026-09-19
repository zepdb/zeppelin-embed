use super::{MemoryError, QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::{
    ValueContext,
    plan::{GraphPlan, PlanBacking, PlanFootprint, RetainedRegion, VALIDATION_SCRATCH_BYTES},
};
use std::marker::PhantomData;

/// Lifetime-retained complete input owner. Numeric regions are not constructors.
/// Nested referenced allocations require their own capabilities. No spare bytes
/// are read, and borrowing prevents reallocating these owners while retained.
#[derive(Clone, Copy)]
pub struct RetainedAllocation<'a> {
    region: RetainedRegion,
    charged_owner: Option<usize>,
    _borrow: PhantomData<&'a ()>,
}
impl<'a> RetainedAllocation<'a> {
    /// Borrows the full actual Vec capacity, including uninitialized spare slots.
    pub fn vector<T>(value: &'a Vec<T>) -> Result<Self, MemoryError> {
        Self::span(
            value.as_ptr() as usize,
            value
                .capacity()
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(MemoryError::Limit)?,
            None,
        )
    }
    /// Borrows the full actual String capacity, including spare bytes.
    pub fn string(value: &'a String) -> Result<Self, MemoryError> {
        Self::span(value.as_ptr() as usize, value.capacity(), None)
    }
    /// Borrows a complete inline array owner; referenced heap backing is separate.
    pub fn array<T, const N: usize>(value: &'a [T; N]) -> Result<Self, MemoryError> {
        Self::span(value.as_ptr() as usize, std::mem::size_of_val(value), None)
    }
    /// Borrows the complete boxed-slice allocation, never a shortened subslice.
    #[allow(
        clippy::borrowed_box,
        reason = "the complete allocation owner is required; an arbitrary slice cannot prove capacity"
    )]
    pub fn boxed<T>(value: &'a Box<[T]>) -> Result<Self, MemoryError> {
        Self::span(
            value.as_ptr() as usize,
            std::mem::size_of_val(value.as_ref()),
            None,
        )
    }
    /// Retains an existing query arena without duplicating its backing charge.
    pub fn arena<T>(value: &'a QueryArena<'_, '_, T>) -> Result<Self, MemoryError> {
        Self::span(
            value.values.as_ptr() as usize,
            value.heap_bytes(),
            Some(value.charge.memory as *const QueryMemory<'_> as usize),
        )
    }
    /// Retains facts only when GraphPlan captured its actual Vec owner before
    /// validation. A raw-slice plan cannot manufacture or upgrade this proof.
    pub fn plan_facts(plan: &'a GraphPlan<'_, '_>) -> Result<Self, MemoryError> {
        Ok(Self {
            region: plan.fact_owner().ok_or(MemoryError::UnprovedInput)?,
            charged_owner: None,
            _borrow: PhantomData,
        })
    }
    fn span(start: usize, bytes: usize, charged_owner: Option<usize>) -> Result<Self, MemoryError> {
        Ok(Self {
            region: RetainedRegion::declared(start, bytes).map_err(|_| MemoryError::Limit)?,
            charged_owner,
            _borrow: PhantomData,
        })
    }
}

/// Complete capacity of the ephemeral capability inventory used during admission.
#[derive(Clone, Copy)]
pub struct RetentionInventory<'p, 'a> {
    entries: &'p [RetainedAllocation<'a>],
    capacity_bytes: usize,
}
impl<'p, 'a> RetentionInventory<'p, 'a> {
    /// Borrows a whole fixed array, with no hidden inventory capacity.
    pub fn array<const N: usize>(entries: &'p [RetainedAllocation<'a>; N]) -> Self {
        Self {
            entries,
            capacity_bytes: std::mem::size_of_val(entries),
        }
    }
    /// Uses actual capacity of the caller-owned inventory vector.
    pub fn vector(entries: &'p Vec<RetainedAllocation<'a>>) -> Result<Self, MemoryError> {
        Ok(Self {
            entries,
            capacity_bytes: entries
                .capacity()
                .checked_mul(std::mem::size_of::<RetainedAllocation<'a>>())
                .ok_or(MemoryError::Limit)?,
        })
    }
}

/// Charged union of lifetime-retained caller allocations and existing query arenas.
/// The inventory is comparison-only; no pointer is ever dereferenced.
pub struct QueryInputs<'m, 'g, 'a> {
    regions: QueryArena<'m, 'g, RetainedRegion>,
    _backing: QueryReservation<'m, 'g>,
    backing_bytes: usize,
    _borrow: PhantomData<&'a ()>,
}
impl<'m, 'g, 'a> QueryInputs<'m, 'g, 'a> {
    /// Reserves full capacities once, including aliases, before accepting spans.
    pub fn reserve(
        memory: &'m QueryMemory<'g>,
        inventory: RetentionInventory<'_, 'a>,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, MemoryError> {
        context.checkpoint()?;
        let _inventory = memory.reserve(inventory.capacity_bytes)?;
        let mut ordered = QueryArena::new(memory, inventory.entries.len())?;
        for entry in inventory.entries {
            context.step()?;
            if entry
                .charged_owner
                .is_some_and(|owner| owner != memory as *const QueryMemory<'_> as usize)
            {
                return Err(MemoryError::UnprovedInput);
            }
            if entry.region.start() != entry.region.end() {
                ordered.push(*entry)?;
            }
        }
        sort(ordered.as_mut_slice(), context)?;
        let mut regions = QueryArena::new(memory, ordered.len())?;
        let mut current: Option<RetainedAllocation<'a>> = None;
        let mut bytes = 0usize;
        let mut all_bytes = 0usize;
        for next in ordered.as_slice() {
            context.step()?;
            if let Some(previous) = &mut current {
                if next.region.start() <= previous.region.end() {
                    if next.region.start() < previous.region.end()
                        && next.charged_owner != previous.charged_owner
                    {
                        return Err(MemoryError::UnprovedInput);
                    }
                    if next.charged_owner == previous.charged_owner {
                        previous.region = RetainedRegion::declared(
                            previous.region.start(),
                            next.region.end().max(previous.region.end()) - previous.region.start(),
                        )
                        .map_err(|_| MemoryError::Limit)?;
                        continue;
                    }
                }
                append(&mut regions, *previous, &mut bytes, &mut all_bytes)?;
            }
            current = Some(*next);
        }
        if let Some(last) = current {
            append(&mut regions, last, &mut bytes, &mut all_bytes)?;
        }
        let controls =
            std::mem::size_of::<Self>() - std::mem::size_of::<QueryArena<'_, '_, RetainedRegion>>();
        let backing = memory.reserve(bytes.checked_add(controls).ok_or(MemoryError::Limit)?)?;
        Ok(Self {
            regions,
            _backing: backing,
            backing_bytes: all_bytes,
            _borrow: PhantomData,
        })
    }
    /// Verifies a typed plan against retained actual-capacity owners and keeps
    /// their reservations alive for the complete execution lifetime.
    pub fn admit_plan<'p, 'plan, 'facts>(
        self,
        plan: &'p GraphPlan<'plan, 'facts>,
        context: &mut ValueContext<'_>,
    ) -> Result<RuntimePlan<'p, 'plan, 'facts, 'm, 'g, 'a>, MemoryError> {
        let controls = std::mem::size_of::<RuntimePlan<'_, '_, '_, '_, '_, '_>>()
            - std::mem::size_of::<Self>()
            + std::mem::size_of_val(plan);
        let control = self._backing.memory.reserve(controls)?;
        let _scratch = self._backing.memory.reserve(VALIDATION_SCRATCH_BYTES)?;
        let inventory = self.regions.heap_bytes();
        let footprint = self
            .backing_bytes
            .checked_add(inventory)
            .and_then(|n| n.checked_add(VALIDATION_SCRATCH_BYTES))
            .ok_or(MemoryError::Limit)?;
        let backing =
            PlanBacking::new(self.regions.as_slice(), inventory).map_err(MemoryError::Plan)?;
        let owner = plan.fact_owner().ok_or(MemoryError::UnprovedInput)?;
        self.verify_region(owner, context)?;
        plan.verify_runtime_backing(PlanFootprint::declared(footprint), backing, context)
            .map_err(MemoryError::Plan)?;
        drop(_scratch);
        Ok(RuntimePlan {
            plan,
            inputs: self,
            _control: control,
        })
    }
    /// Full union of retained backing capacities; existing arena charges are shared.
    pub const fn backing_bytes(&self) -> usize {
        self.backing_bytes
    }
    /// Checks visible inclusion only; actual capacity comes from retained owners.
    pub fn verify_span<T>(
        &self,
        values: &[T],
        context: &mut ValueContext<'_>,
    ) -> Result<(), MemoryError> {
        context.checkpoint()?;
        self.verify_region(
            RetainedRegion::slice(values).map_err(|_| MemoryError::Limit)?,
            context,
        )
    }
    fn verify_region(
        &self,
        span: RetainedRegion,
        context: &mut ValueContext<'_>,
    ) -> Result<(), MemoryError> {
        let start = span.start();
        let end = span.end();
        if start == end {
            return Ok(());
        }
        let mut low = 0;
        let mut high = self.regions.len();
        while low < high {
            context.step()?;
            let mid = low + (high - low) / 2;
            if self
                .regions
                .as_slice()
                .get(mid)
                .ok_or(MemoryError::UnprovedInput)?
                .end()
                <= start
            {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let mut position = start;
        while position < end {
            context.step()?;
            let region = self
                .regions
                .as_slice()
                .get(low)
                .ok_or(MemoryError::UnprovedInput)?;
            if region.start() > position || region.end() <= position {
                return Err(MemoryError::UnprovedInput);
            }
            position = region.end();
            low += 1;
        }
        Ok(())
    }
}
fn append(
    regions: &mut QueryArena<'_, '_, RetainedRegion>,
    entry: RetainedAllocation<'_>,
    bytes: &mut usize,
    all_bytes: &mut usize,
) -> Result<(), MemoryError> {
    let length = entry.region.end() - entry.region.start();
    if entry.charged_owner.is_none() {
        *bytes = bytes.checked_add(length).ok_or(MemoryError::Limit)?;
    }
    *all_bytes = all_bytes.checked_add(length).ok_or(MemoryError::Limit)?;
    regions.push(entry.region)
}
fn sort(
    values: &mut [RetainedAllocation<'_>],
    context: &mut ValueContext<'_>,
) -> Result<(), MemoryError> {
    for root in (0..values.len() / 2).rev() {
        sift(values, root, values.len(), context)?;
    }
    for end in (1..values.len()).rev() {
        context.step()?;
        values.swap(0, end);
        sift(values, 0, end, context)?;
    }
    Ok(())
}
fn sift(
    values: &mut [RetainedAllocation<'_>],
    mut root: usize,
    end: usize,
    context: &mut ValueContext<'_>,
) -> Result<(), MemoryError> {
    while root < end / 2 {
        context.step()?;
        let mut child = root * 2 + 1;
        let key = |i: usize| {
            values
                .get(i)
                .map(|e| (e.region.start(), e.region.end()))
                .ok_or(MemoryError::UnprovedInput)
        };
        if child + 1 < end && key(child)? < key(child + 1)? {
            child += 1;
        }
        if key(root)? >= key(child)? {
            break;
        }
        values.swap(root, child);
        root = child;
    }
    Ok(())
}

/// A typed plan plus its actual lifetime-retained backing reservations.
/// Construction is private to complete owner/span verification.
pub struct RuntimePlan<'p, 'plan, 'facts, 'm, 'g, 'a> {
    plan: &'p GraphPlan<'plan, 'facts>,
    inputs: QueryInputs<'m, 'g, 'a>,
    _control: QueryReservation<'m, 'g>,
}
impl<'p, 'plan, 'facts> RuntimePlan<'p, 'plan, 'facts, '_, '_, '_> {
    pub(crate) fn belongs_to(&self, memory: &QueryMemory<'_>) -> bool {
        std::ptr::eq(self.inputs._backing.memory, memory)
    }
    /// Returns the original immutable plan; no new structural facts are inferred.
    pub const fn plan(&self) -> &'p GraphPlan<'plan, 'facts> {
        self.plan
    }
    /// Actual retained backing union, including shared existing query arenas.
    pub const fn backing_bytes(&self) -> usize {
        self.inputs.backing_bytes
    }
}

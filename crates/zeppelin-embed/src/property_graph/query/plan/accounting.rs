use super::super::MAX_QUERY_BYTES;
use super::*;
/// Owner-attested retained address interval. Addresses are compared only and
/// never dereferenced. Capacity/allocator ownership comes from the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RetainedRegion {
    start: usize,
    end: usize,
}
impl RetainedRegion {
    /// Records a complete borrowed span; the owner must separately include any
    /// retained allocation capacity outside that span.
    pub fn slice<T>(values: &[T]) -> Result<Self, PlanError> {
        Self::declared(values.as_ptr() as usize, std::mem::size_of_val(values))
    }
    /// Records the actual capacity of an owned Vec without reading spare memory.
    pub fn vector<T>(values: &Vec<T>) -> Result<Self, PlanError> {
        Self::declared(
            values.as_ptr() as usize,
            values
                .capacity()
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(PlanError::Footprint)?,
        )
    }
    /// Numeric capacity declaration for an external arena; not proof of ownership.
    pub fn declared(start: usize, capacity_bytes: usize) -> Result<Self, PlanError> {
        Ok(Self {
            start,
            end: start
                .checked_add(capacity_bytes)
                .ok_or(PlanError::Footprint)?,
        })
    }
    /// Inclusive address for sorting/merging inventories outside validation.
    pub const fn start(self) -> usize {
        self.start
    }
    /// Exclusive checked endpoint.
    pub const fn end(self) -> usize {
        self.end
    }
}
/// Borrowed sorted, disjoint retained-region proof. Inventory storage is charged
/// separately at declared full capacity, and may not overlap any retained region.
/// The fixed validator stack envelope is also charged separately. Zero-length inventory regions are rejected; empty visible
/// spans require no region. adjacent regions may jointly cover one visible span.
#[derive(Clone, Copy, Debug)]
pub struct PlanBacking<'a> {
    regions: &'a [RetainedRegion],
    inventory_capacity_bytes: usize,
}
impl<'a> PlanBacking<'a> {
    /// Attests full inventory backing capacity; visible length is checked here.
    pub fn new(
        regions: &'a [RetainedRegion],
        inventory_capacity_bytes: usize,
    ) -> Result<Self, PlanError> {
        if inventory_capacity_bytes < std::mem::size_of_val(regions) {
            return Err(PlanError::Footprint);
        }
        RetainedRegion::declared(regions.as_ptr() as usize, inventory_capacity_bytes)?;
        Ok(Self {
            regions,
            inventory_capacity_bytes,
        })
    }
    /// Uses actual Vec capacity for the inventory itself.
    pub fn vector(regions: &'a Vec<RetainedRegion>) -> Result<Self, PlanError> {
        Self::new(
            regions,
            regions
                .capacity()
                .checked_mul(std::mem::size_of::<RetainedRegion>())
                .ok_or(PlanError::Footprint)?,
        )
    }
}
pub(super) struct Accounting<'a> {
    regions: &'a [RetainedRegion],
}
impl<'a> Accounting<'a> {
    pub(super) fn new(
        backing: PlanBacking<'a>,
        footprint: PlanFootprint,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, PlanError> {
        let inventory = RetainedRegion::declared(
            backing.regions.as_ptr() as usize,
            backing.inventory_capacity_bytes,
        )?;
        let mut bytes = VALIDATION_SCRATCH_BYTES
            .checked_add(backing.inventory_capacity_bytes)
            .ok_or(PlanError::Footprint)?;
        let mut previous_end = 0;
        for region in backing.regions {
            context.step()?;
            if region.start == region.end {
                return Err(PlanError::Footprint);
            }
            if region.start < previous_end
                || region.start < inventory.end && inventory.start < region.end
            {
                return Err(PlanError::Footprint);
            }
            previous_end = region.end;
            bytes = bytes
                .checked_add(region.end - region.start)
                .ok_or(PlanError::Footprint)?;
        }
        if bytes > footprint.retained_bytes || footprint.retained_bytes > MAX_QUERY_BYTES {
            return Err(PlanError::Footprint);
        }
        Ok(Self {
            regions: backing.regions,
        })
    }
    pub(super) fn span<T>(
        &self,
        values: &[T],
        context: &mut ValueContext<'_>,
    ) -> Result<(), PlanError> {
        let span = RetainedRegion::slice(values)?;
        if span.start == span.end {
            return Ok(());
        }
        let mut low = 0;
        let mut high = self.regions.len();
        while low < high {
            context.step()?;
            let mid = low + (high - low) / 2;
            if self.regions.get(mid).ok_or(PlanError::Footprint)?.end <= span.start {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let mut position = span.start;
        while position < span.end {
            context.step()?;
            let region = self.regions.get(low).ok_or(PlanError::Footprint)?;
            if region.start > position || region.end <= position {
                return Err(PlanError::Footprint);
            }
            position = region.end;
            low += 1;
        }
        Ok(())
    }
}

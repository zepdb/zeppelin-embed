//! Private admission precedes publication. The intrusive list allocates exactly
//! one node per result; lookup is O(N), capped by the configured admission bound.
use super::*;
use std::cell::UnsafeCell;
use std::mem::{ManuallyDrop, size_of};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// Shared across registry instances, including all-empty arenas.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

struct Node {
    token: u64,
    published: AtomicBool,
    root: UnsafeCell<ZeGraphResponse>,
    arena: AlignedArena,
    next: *mut Node,
}
struct List {
    head: *mut Node,
    len: usize,
}
// SAFETY: links and removals are exclusively protected by the registry gate.
// Before publication only the unique PreparedResponse accesses root. After
// release/acquire publication root and backing are immutable until removal.
unsafe impl Send for List {}

// An embedded gate avoids std Mutex's lazy, infallible 64-byte allocation on
// macOS. Normal calls never wait: contention is a typed precommit/free error.
struct Gate {
    held: AtomicBool,
    poisoned: AtomicBool,
    value: UnsafeCell<List>,
}
unsafe impl Sync for Gate {} // exclusive acquire/release guard owns List
impl Gate {
    const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            value: UnsafeCell::new(List {
                head: std::ptr::null_mut(),
                len: 0,
            }),
        }
    }
    fn acquire(&self) -> Result<GateGuard<'_>, OwnerError> {
        self.held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| OwnerError::Busy)?;
        Ok(GateGuard {
            gate: self,
            entered_panicking: std::thread::panicking(),
        })
    }
    fn try_lock(&self) -> Result<GateGuard<'_>, OwnerError> {
        let guard = self.acquire()?;
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(OwnerError::Poisoned);
        }
        Ok(guard)
    }
    fn cleanup_lock(&self) -> GateGuard<'_> {
        // Only abort cleanup waits. Every critical section is bounded by the
        // explicit registry limit and contains no allocation or callback.
        loop {
            if let Ok(guard) = self.acquire() {
                return guard;
            }
            std::thread::yield_now();
        }
    }
}
struct GateGuard<'a> {
    gate: &'a Gate,
    entered_panicking: bool,
}
impl Deref for GateGuard<'_> {
    type Target = List;
    fn deref(&self) -> &List {
        unsafe { &*self.gate.value.get() }
    }
}
impl DerefMut for GateGuard<'_> {
    fn deref_mut(&mut self) -> &mut List {
        unsafe { &mut *self.gate.value.get() }
    }
}
impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        if !self.entered_panicking && std::thread::panicking() {
            self.gate.poisoned.store(true, Ordering::Relaxed);
        }
        self.gate.held.store(false, Ordering::Release);
    }
}

/// Process-lifetime result registry, isolated from all legacy result types.
/// Admission has an explicit outstanding-node bound. Free performs at most that
/// many comparisons, then releases two allocations without recursive traversal.
/// No default public C limit is chosen by this internal component.
pub struct GraphResultRegistry {
    list: Gate,
    max_outstanding: usize,
}
impl GraphResultRegistry {
    /// Declares a static registry and explicit admission bound. Zero rejects all
    /// preparations. The registry itself allocates no heap slab or hash table.
    pub const fn new(max_outstanding: usize) -> Self {
        Self {
            list: Gate::new(),
            max_outstanding,
        }
    }
    /// Copies aligned typed pools while source owners remain live, reserves the
    /// actual arena/node/control capacities, then admits a private registry node.
    /// The caller must prove/charge whole source owners at real ZE-68 conversion.
    pub fn prepare<'m, 'g>(
        &'static self,
        context: &mut RuntimeContext<'_, 'm, 'g>,
        parts: ResponseParts<'_>,
        metadata: ResponseMetadata,
    ) -> Result<PreparedResponse<'m, 'g>, OwnerError> {
        context.checkpoint()?;
        if metadata.row_count > 65536
            || parts.columns.len() > 256
            || metadata.row_count.checked_mul(parts.columns.len()) != Some(parts.cells.len())
            || (metadata.global_work.start as usize)
                .checked_add(metadata.global_work.count as usize)
                .is_none_or(|end| end > parts.work.len())
        {
            return Err(OwnerError::InvalidShape);
        }
        let plan = ArenaLayout::new(parts.counts())?;
        // Exhaustion and poison are typed precommit errors. Tokens are burned
        // on later failure; they never wrap, reset, or get reused.
        let token = NEXT_TOKEN
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| OwnerError::TokenExhausted)?;
        {
            let list = self.list.try_lock()?;
            if list.len >= self.max_outstanding {
                return Err(OwnerError::RegistryFull);
            }
        }
        let mut charge = context.memory().reserve_external_capacity()?;
        let controls = size_of::<PreparedResponse<'_, '_>>()
            - size_of::<QueryExternalReservation<'_, '_>>()
            + size_of::<ArenaLayout>()
            + size_of::<ResponseParts<'_>>()
            + size_of::<ResponseMetadata>();
        let bytes = plan
            .layout
            .size()
            .checked_add(size_of::<Node>())
            .and_then(|n| n.checked_add(controls))
            .ok_or(OwnerError::Limit)?;
        charge.reserve_additional(bytes)?;
        let arena = AlignedArena::allocate(plan.layout)?;
        let mut root = fill(&arena, &plan, parts, metadata, context)?;
        root.owner_token = token;
        context.checkpoint()?;
        let raw = NonNull::new(unsafe { allocate_raw(Layout::new::<Node>()) })
            .ok_or(OwnerError::Allocation)?
            .cast::<Node>();
        // The fallible raw allocation is initialized immediately; OwnedNode
        // covers every subsequent error/panic until list admission succeeds.
        unsafe {
            raw.as_ptr().write(Node {
                token,
                published: AtomicBool::new(false),
                root: UnsafeCell::new(root),
                arena,
                next: std::ptr::null_mut(),
            });
        }
        let node = OwnedNode(raw);
        let mut list = self.list.try_lock()?;
        if list.len >= self.max_outstanding {
            return Err(OwnerError::RegistryFull);
        }
        unsafe {
            (*raw.as_ptr()).next = list.head;
        }
        list.head = raw.as_ptr();
        list.len += 1; // checked against the explicit usize admission bound
        std::mem::forget(node);
        drop(list);
        let prepared = PreparedResponse {
            registry: self,
            node: raw,
            charge,
        };
        context.checkpoint()?;
        Ok(prepared)
    }
    /// Validates every immutable root/pool field against authoritative geometry.
    /// A private node, stale token, foreign type, or changed field cannot steal
    /// backing. Caller pointers are compared as values and never dereferenced.
    /// On success the caller descriptor becomes canonical empty; repeat free is
    /// a successful no-op. Gate poison remains a typed error, preserving owner.
    pub fn free(&self, root: &mut ZeGraphResponse) -> Result<FreeReport, OwnerError> {
        if root.owner_token == 0 {
            return if same_geometry(root, &empty_response()) {
                Ok(FreeReport {
                    examined_entries: 0,
                    released_bytes: 0,
                })
            } else {
                Err(OwnerError::InvalidOwner)
            };
        }
        let mut list = self.list.try_lock()?;
        let mut link: *mut *mut Node = &mut list.head;
        let mut examined_entries = 0;
        // SAFETY: all links are exclusively owned by this exclusively held list. Private
        // nodes cannot disappear except under this same gate. Published roots
        // are immutable after acquire; the entire caller geometry is compared.
        unsafe {
            while !(*link).is_null() {
                let node = *link;
                examined_entries += 1;
                if (*node).token == root.owner_token {
                    if !(*node).published.load(Ordering::Acquire)
                        || !same_geometry(&*(*node).root.get(), root)
                    {
                        return Err(OwnerError::InvalidOwner);
                    }
                    // The caller root itself must survive clearing after free.
                    // Use registered full extents, never host-reported counts.
                    let root_address = root as *mut ZeGraphResponse as usize;
                    if overlaps(
                        root_address,
                        size_of::<ZeGraphResponse>(),
                        (*node).arena.pointer.as_ptr() as usize,
                        (*node).arena.layout.size(),
                    ) || overlaps(
                        root_address,
                        size_of::<ZeGraphResponse>(),
                        node as usize,
                        size_of::<Node>(),
                    ) {
                        return Err(OwnerError::InvalidOwner);
                    }
                    *link = (*node).next;
                    list.len -= 1;
                    let released_bytes = (*node).arena.layout.size() + size_of::<Node>();
                    drop(list);
                    drop(OwnedNode(NonNull::new_unchecked(node)));
                    *root = empty_response();
                    return Ok(FreeReport {
                        examined_entries,
                        released_bytes,
                    });
                }
                link = &mut (*node).next;
            }
        }
        Err(OwnerError::InvalidOwner)
    }
    fn abort(&self, pointer: NonNull<Node>) {
        // Cleanup only: retain poison for every subsequent normal operation.
        // Drop cannot report an error and cannot leave its private allocation
        // live after its temporary accounting guard disappears.
        let mut list = self.list.cleanup_lock();
        let mut link: *mut *mut Node = &mut list.head;
        unsafe {
            while !(*link).is_null() {
                if *link == pointer.as_ptr() {
                    *link = (*pointer.as_ptr()).next;
                    list.len -= 1;
                    drop(list);
                    drop(OwnedNode(pointer));
                    return;
                }
                link = &mut (**link).next;
            }
        }
    }
}
struct OwnedNode(NonNull<Node>);
impl Drop for OwnedNode {
    fn drop(&mut self) {
        unsafe {
            std::ptr::drop_in_place(self.0.as_ptr());
            dealloc(self.0.as_ptr().cast(), Layout::new::<Node>());
        }
    }
}
/// One successful free observation; lookup and destruction costs are distinct.
#[derive(Debug, Eq, PartialEq)]
pub struct FreeReport {
    /// Actual registry entries examined, bounded by configured admission count.
    pub examined_entries: usize,
    /// Actual system allocation sizes released (arena padding plus node).
    pub released_bytes: usize,
}
/// Unique right to abort or publish an already allocated and registered owner.
/// Its real query guard cannot outlive the query; the exposed C owner has no
/// query, store, snapshot, or caller-input lifetime.
pub struct PreparedResponse<'m, 'g> {
    registry: &'static GraphResultRegistry,
    node: NonNull<Node>,
    charge: QueryExternalReservation<'m, 'g>,
}
impl PreparedResponse<'_, '_> {
    /// Read-only descriptor snapshot. Free rejects it until publication.
    pub fn descriptor(&self) -> ZeGraphResponse {
        // Unique prepared owner; root is still private and cannot be freed.
        unsafe { *(*self.node.as_ptr()).root.get() }
    }
    /// Measured full padded ABI arena capacity, separate from registry controls.
    pub fn arena_bytes(&self) -> usize {
        unsafe { (*self.node.as_ptr()).arena.layout.size() }
    }
    /// Measured represented bytes, excluding padding and registry controls.
    /// The coordinator charges CompletedAbiBytes once using this measurement.
    pub fn represented_bytes(&self) -> usize {
        let root = self.descriptor();
        root.pool.value_count * size_of::<ZeGraphValue>()
            + root.pool.child_count * 4
            + root.pool.byte_count
            + root.pool.node_count * size_of::<ZeGraphNode>()
            + root.pool.relationship_count * size_of::<ZeGraphRelationship>()
            + root.pool.property_count * size_of::<ZeGraphProperty>()
            + root.pool.name_count * size_of::<ZeGraphRange>()
            + root.pool.vector_count * 4
            + root.column_count * size_of::<ZeGraphColumn>()
            + root.cell_count * 4
            + root.receipt_count * size_of::<ZeGraphReceipt>()
            + root.report_count * size_of::<ZeGraphSearchReport>()
            + root.diagnostic_count * size_of::<ZeGraphDiagnostic>()
            + root.work_count * size_of::<ZeGraphWorkCounter>()
    }
    /// Actual retained heap bytes: padded arena plus registry node.
    pub fn allocation_bytes(&self) -> usize {
        self.arena_bytes() + size_of::<Node>()
    }
    /// Complete temporary reservation, including explicit control descriptors.
    pub fn reserved_bytes(&self) -> usize {
        self.charge.bytes() + size_of::<QueryExternalReservation<'_, '_>>()
    }
    /// Infallible fixed-metadata publication. Caller establishes the authentic
    /// successful outcome. No registry/fallible gate, allocation, payload copy
    /// or callback occurs. Existing accounting guard Drop takes its infallible
    /// cleanup mutex; this transition is not claimed to be lock-free.
    pub fn expose(self, outcome: SuccessfulOutcome) -> ZeGraphResponse {
        let prepared = ManuallyDrop::new(self); // disarm abort before publication
        // SAFETY: unique prepared ownership excludes another expose/abort;
        // acquire-side free refuses the private node. Move the charge out and
        // copy metadata before release-store: a concurrent free may destroy
        // the node immediately afterward, so never dereference it again.
        unsafe {
            let charge = std::ptr::read(&prepared.charge);
            let node = prepared.node.as_ptr();
            outcome.apply(&mut *(*node).root.get());
            let root = *(*node).root.get();
            (*node).published.store(true, Ordering::Release);
            drop(charge);
            root
        }
    }
}
impl Drop for PreparedResponse<'_, '_> {
    fn drop(&mut self) {
        self.registry.abort(self.node);
    }
}

fn overlaps(first: usize, first_bytes: usize, second: usize, second_bytes: usize) -> bool {
    if first_bytes == 0 || second_bytes == 0 {
        return false;
    }
    match (
        first.checked_add(first_bytes),
        second.checked_add(second_bytes),
    ) {
        (Some(first_end), Some(second_end)) => first < second_end && second < first_end,
        _ => true,
    }
}

fn same_geometry(a: &ZeGraphResponse, b: &ZeGraphResponse) -> bool {
    macro_rules! same { ($a:expr, $b:expr; $($field:ident),+ $(,)?) => { true $(&& $a.$field == $b.$field)+ }; }
    same!(a, b; abi_size, abi_reserved, owner_token, disposition, has_admitted_generation, admitted_generation,
        has_changed_generation, reserved, changed_generation, row_count, columns, column_count, cells, cell_count,
        receipts, receipt_count, reports, report_count, diagnostics, diagnostic_count, work, work_count, global_work)
        && same!(a.pool, b.pool; abi_size, abi_reserved, values, value_count, children, child_count, bytes, byte_count,
        nodes, node_count, relationships, relationship_count, properties, property_count, names, name_count, vectors, vector_count)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::super::tests::with_context;
    use super::*;

    #[test]
    fn graph_result_gate_poison_busy_and_registry_bound_are_precommit_errors() {
        static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
        with_context(|context| {
            let baseline = context.memory().reserved_bytes();
            let prepared = REGISTRY
                .prepare(
                    context,
                    ResponseParts::default(),
                    ResponseMetadata::new(0, None),
                )
                .unwrap();
            assert!(matches!(
                REGISTRY.prepare(
                    context,
                    ResponseParts::default(),
                    ResponseMetadata::new(0, None)
                ),
                Err(OwnerError::RegistryFull)
            ));
            let guard = REGISTRY.list.try_lock().unwrap();
            assert!(matches!(
                REGISTRY.prepare(
                    context,
                    ResponseParts::default(),
                    ResponseMetadata::new(0, None)
                ),
                Err(OwnerError::Busy)
            ));
            drop(guard);
            let unwind = std::panic::catch_unwind(|| {
                let _guard = REGISTRY.list.try_lock().unwrap();
                panic!("injected gate unwind");
            });
            assert!(unwind.is_err());
            assert!(matches!(
                REGISTRY.prepare(
                    context,
                    ResponseParts::default(),
                    ResponseMetadata::new(0, None)
                ),
                Err(OwnerError::Poisoned)
            ));
            let (_, cleanup) = super::super::audit::run(0, true, || drop(prepared));
            assert_eq!(cleanup.attempts, 0);
            assert_eq!(cleanup.frees, 1); // empty arena never allocates
            assert_eq!(context.memory().reserved_bytes(), baseline);
            assert!(REGISTRY.list.poisoned.load(Ordering::Relaxed));
            assert_eq!(REGISTRY.list.cleanup_lock().len, 0);
        });
    }

    #[test]
    fn graph_result_token_exhaustion_precedes_allocation_or_exposure() {
        // Nextest gives this mutation its own process (one libtest thread).
        static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
        with_context(|context| {
            let previous = NEXT_TOKEN.swap(u64::MAX, Ordering::Relaxed);
            let (result, audit) = super::super::audit::run(0, true, || {
                REGISTRY.prepare(
                    context,
                    ResponseParts::default(),
                    ResponseMetadata::new(0, None),
                )
            });
            NEXT_TOKEN.store(previous, Ordering::Relaxed);
            assert!(matches!(result, Err(OwnerError::TokenExhausted)));
            assert_eq!(audit.attempts, 0);
            assert_eq!(REGISTRY.list.cleanup_lock().len, 0);
        });
    }
}

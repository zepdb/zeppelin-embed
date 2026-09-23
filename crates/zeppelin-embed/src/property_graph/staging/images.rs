//! Append-only, per-statement arena for the full replacement images a
//! query-driven mutation hands to the writer overlay.
//!
//! `GraphBatchReadView::replace` borrows its image for the overlay's whole
//! lifetime, but a mutation executor builds each image from values that live
//! only as long as one expression evaluation. Every image is therefore copied
//! into this arena first. The arena lives beside the overlay for the whole
//! statement, never frees or moves an image it has handed out, and refuses a
//! new image once its declared capacity is used: a hard cap, never eviction.
//!
//! # Memory safety
//!
//! Each image owns its own bounded buffers, one `Arena` per field, each sized
//! exactly once from a caller-measured budget. An `Arena` never reallocates:
//! its capacity is reserved once with `try_reserve_exact`, and `push`/`write`
//! refuse anything past that capacity instead of growing. The descriptors an
//! image holds (`GraphName`, `&str`, `GraphProperty`, `CanonicalContents`)
//! point into those heap buffers and are typed with the arena lifetime `'i`
//! even though the buffers live only as long as the `StatementImages` that
//! owns them. They are never handed out at `'i`: every public accessor
//! returns them bounded by the `&'w self` borrow of the arena, so no reference
//! can outlive the buffers it names. This is the same "heap-stable owner,
//! borrow bounded by the owner" argument `CachedPropertyValue::Strings` and
//! slice B's `LazyTargets` rely on in `lifecycle::native_graph::base`.
//!
//! Two further properties are specific to this arena.
//!
//! First, an image's buffers are never shared with another image. A finished
//! image is moved, as a whole `NodeImageSlot`/`RelationshipImageSlot` value,
//! into a fixed-capacity outer array. That move copies only the `Arena`
//! headers; the heap buffers the descriptors point into stay where they are.
//! A later image only appends to the outer array, which writes the outer
//! array's own buffer and never the inner heap buffers an earlier image's
//! borrower still reads. The finished `CanonicalContents` itself lives in a
//! one-element inner buffer for the same reason.
//!
//! Second, `CanonicalContents::node` sorts and deduplicates its descriptors in
//! place, so it needs `&'i mut` label and property slices. `finish` takes
//! those mutable slices from the image's own buffers exactly once. They are
//! consumed entirely inside `CanonicalContents::node`, which downgrades them
//! to the shared `&'i` slices the resulting image retains, and nothing ever
//! reads or writes those two buffers through their `Arena` again. The only
//! later access is `Drop`, which runs after every `&'w self` borrow has
//! ended. At no point do a mutable and a shared reference to the same
//! descriptor coexist.

use super::memory::Arena;
use super::*;
use crate::epoch::EmbeddingTower;
use std::cell::RefCell;
use std::io::Write;

/// Measured sizes for one node image, taken before any buffer is reserved.
/// Every field is an exact element or byte count, never an estimate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeImageBudget {
    /// Label descriptors.
    pub labels: usize,
    /// Copied label name bytes.
    pub label_bytes: usize,
    /// Property descriptors.
    pub properties: usize,
    /// Copied property name bytes.
    pub name_bytes: usize,
    /// Copied string value bytes, scalar and list elements together.
    pub string_bytes: usize,
    /// String-list element descriptors.
    pub string_views: usize,
    /// Integer-list elements.
    pub integers: usize,
    /// Float-list elements.
    pub floats: usize,
    /// Boolean-list elements.
    pub bools: usize,
    /// Present stored text and its byte length; `None` is absent text.
    pub text_bytes: Option<usize>,
    /// Present vector and its dimension count; `None` is no vector.
    pub vector_dims: Option<u32>,
}

/// Measured sizes for one relationship image.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelationshipImageBudget {
    /// Property descriptors.
    pub properties: usize,
    /// Copied property name bytes.
    pub name_bytes: usize,
    /// Copied string value bytes, scalar and list elements together.
    pub string_bytes: usize,
    /// String-list element descriptors.
    pub string_views: usize,
    /// Integer-list elements.
    pub integers: usize,
    /// Float-list elements.
    pub floats: usize,
    /// Boolean-list elements.
    pub bools: usize,
    /// Copied relationship type bytes.
    pub type_bytes: usize,
}

/// The property-value share of an image budget, counted identically for
/// nodes and relationships so the measuring pass and the copy agree.
#[derive(Clone, Copy, Default)]
struct PropertyBudget {
    properties: usize,
    name_bytes: usize,
    string_bytes: usize,
    string_views: usize,
    integers: usize,
    floats: usize,
    bools: usize,
}

impl PropertyBudget {
    fn add(&mut self, name: GraphName<'_>, value: PropertyValue<'_>) -> Result<(), StageError> {
        let add = |total: &mut usize, count: usize| -> Result<(), StageError> {
            *total = total.checked_add(count).ok_or(StageError::Limit)?;
            Ok(())
        };
        add(&mut self.properties, 1)?;
        add(&mut self.name_bytes, name.as_str().len())?;
        match value.data() {
            PropertyData::String(value) => add(&mut self.string_bytes, value.len())?,
            PropertyData::Strings(values) => {
                add(&mut self.string_views, values.len())?;
                for value in values {
                    add(&mut self.string_bytes, value.len())?;
                }
            }
            PropertyData::Bools(values) => add(&mut self.bools, values.len())?,
            PropertyData::Integers(values) => add(&mut self.integers, values.len())?,
            PropertyData::Floats(values) => add(&mut self.floats, values.len())?,
            PropertyData::Bool(_)
            | PropertyData::I64(_)
            | PropertyData::F64(_)
            | PropertyData::EmptyList { .. } => {}
        }
        Ok(())
    }
}

impl NodeImageBudget {
    /// Counts one label the copy pass will add.
    pub fn label(&mut self, name: GraphName<'_>) -> Result<(), StageError> {
        self.labels = self.labels.checked_add(1).ok_or(StageError::Limit)?;
        self.label_bytes = self
            .label_bytes
            .checked_add(name.as_str().len())
            .ok_or(StageError::Limit)?;
        Ok(())
    }

    /// Counts one property the copy pass will add.
    pub fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
    ) -> Result<(), StageError> {
        let mut values = self.values();
        values.add(name, value)?;
        self.properties = values.properties;
        self.name_bytes = values.name_bytes;
        self.string_bytes = values.string_bytes;
        self.string_views = values.string_views;
        self.integers = values.integers;
        self.floats = values.floats;
        self.bools = values.bools;
        Ok(())
    }

    const fn values(&self) -> PropertyBudget {
        PropertyBudget {
            properties: self.properties,
            name_bytes: self.name_bytes,
            string_bytes: self.string_bytes,
            string_views: self.string_views,
            integers: self.integers,
            floats: self.floats,
            bools: self.bools,
        }
    }
}

impl RelationshipImageBudget {
    /// Counts one property the copy pass will add.
    pub fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
    ) -> Result<(), StageError> {
        let mut values = self.values();
        values.add(name, value)?;
        self.properties = values.properties;
        self.name_bytes = values.name_bytes;
        self.string_bytes = values.string_bytes;
        self.string_views = values.string_views;
        self.integers = values.integers;
        self.floats = values.floats;
        self.bools = values.bools;
        Ok(())
    }

    const fn values(&self) -> PropertyBudget {
        PropertyBudget {
            properties: self.properties,
            name_bytes: self.name_bytes,
            string_bytes: self.string_bytes,
            string_views: self.string_views,
            integers: self.integers,
            floats: self.floats,
            bools: self.bools,
        }
    }
}

/// Every byte one image owns: property names and values, and the property
/// descriptors that point into them.
struct PropertySlot<'i> {
    properties: Arena<'i, GraphProperty<'i>>,
    names: Arena<'i, u8>,
    strings: Arena<'i, u8>,
    views: Arena<'i, &'i str>,
    integers: Arena<'i, i64>,
    floats: Arena<'i, f64>,
    bools: Arena<'i, bool>,
}

struct NodeImageSlot<'i> {
    values: PropertySlot<'i>,
    labels: Arena<'i, GraphName<'i>>,
    label_bytes: Arena<'i, u8>,
    text: Arena<'i, u8>,
    vector: Arena<'i, f32>,
    contents: Arena<'i, CanonicalContents<'i>>,
}

struct RelationshipImageSlot<'i> {
    values: PropertySlot<'i>,
    type_bytes: Arena<'i, u8>,
}

/// Retains a view into one image-owned buffer at the arena lifetime.
///
/// # Safety
///
/// `values` must point into the heap buffer of an `Arena` that belongs to an
/// image slot of this module. The caller must never write that range again
/// and must expose the result only through a borrow of the owning
/// `StatementImages`; see the module documentation.
unsafe fn retained<'i, T>(values: &[T]) -> &'i [T] {
    let pointer: *const [T] = values;
    // SAFETY: the caller guarantees the module-level contract: the range is
    // heap-stable, never written again, and never observed past its owner.
    unsafe { &*pointer }
}

/// Copies `bytes` into one image-owned byte buffer, polling between chunks,
/// and returns the retained UTF-8 view of the copy.
fn copy_str<'i>(
    buffer: &mut Arena<'i, u8>,
    value: &str,
    control: &mut WriteControl<'_>,
) -> Result<&'i str, StageError> {
    let start = buffer.len();
    for chunk in value.as_bytes().chunks(64 * 1024) {
        control(WritePhase::Overlay)?;
        buffer.write_all(chunk).map_err(|_| StageError::Limit)?;
    }
    let copied = buffer
        .get(start..start.checked_add(value.len()).ok_or(StageError::Limit)?)
        .ok_or(StageError::InvalidInput)?;
    // SAFETY: `copied` is the range just written into this image's own
    // fixed-capacity byte buffer, which is appended to but never rewritten or
    // reallocated; the view is only exposed through the owning arena.
    let copied = unsafe { retained(copied) };
    std::str::from_utf8(copied).map_err(|_| StageError::InvalidInput)
}

/// Copies a list into one image-owned element buffer and returns the
/// retained slice of the copy.
fn copy_list<'i, T: Copy>(
    buffer: &mut Arena<'i, T>,
    values: &[T],
    control: &mut WriteControl<'_>,
) -> Result<&'i [T], StageError> {
    let start = buffer.len();
    for chunk in values.chunks(4096) {
        control(WritePhase::Overlay)?;
        for value in chunk {
            buffer.push(*value)?;
        }
    }
    let copied = buffer
        .get(start..start.checked_add(values.len()).ok_or(StageError::Limit)?)
        .ok_or(StageError::InvalidInput)?;
    // SAFETY: as in `copy_str`: the range was just appended to this image's
    // own fixed-capacity buffer and is never rewritten or reallocated.
    Ok(unsafe { retained(copied) })
}

impl<'i> PropertySlot<'i> {
    fn new(
        memory: &'i WriteMemory<'i>,
        budget: PropertyBudget,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        Ok(Self {
            properties: Arena::new(memory, budget.properties, control)?,
            names: Arena::new(memory, budget.name_bytes, control)?,
            strings: Arena::new(memory, budget.string_bytes, control)?,
            views: Arena::new(memory, budget.string_views, control)?,
            integers: Arena::new(memory, budget.integers, control)?,
            floats: Arena::new(memory, budget.floats, control)?,
            bools: Arena::new(memory, budget.bools, control)?,
        })
    }

    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        let name = GraphName::new(copy_str(&mut self.names, name.as_str(), control)?)
            .map_err(|_| StageError::InvalidInput)?;
        let data = match value.data() {
            PropertyData::String(value) => {
                PropertyData::String(copy_str(&mut self.strings, value, control)?)
            }
            PropertyData::Bool(value) => PropertyData::Bool(value),
            PropertyData::I64(value) => PropertyData::I64(value),
            PropertyData::F64(value) => PropertyData::F64(value),
            PropertyData::EmptyList { count } => PropertyData::EmptyList { count },
            PropertyData::Strings(values) => {
                let start = self.views.len();
                for value in values {
                    let copied = copy_str(&mut self.strings, value, control)?;
                    self.views.push(copied)?;
                }
                let views = self
                    .views
                    .get(start..start.checked_add(values.len()).ok_or(StageError::Limit)?)
                    .ok_or(StageError::InvalidInput)?;
                // SAFETY: the descriptors were just appended to this image's
                // own fixed-capacity buffer and are never rewritten.
                PropertyData::Strings(unsafe { retained(views) })
            }
            PropertyData::Bools(values) => {
                PropertyData::Bools(copy_list(&mut self.bools, values, control)?)
            }
            PropertyData::Integers(values) => {
                PropertyData::Integers(copy_list(&mut self.integers, values, control)?)
            }
            PropertyData::Floats(values) => {
                PropertyData::Floats(copy_list(&mut self.floats, values, control)?)
            }
        };
        let value = PropertyValue::new(data).map_err(|_| StageError::InvalidInput)?;
        self.properties.push(GraphProperty::new(name, value))
    }
}

/// One statement's append-only image arena.
pub struct StatementImages<'i> {
    memory: &'i WriteMemory<'i>,
    document: Option<&'i EmbeddingTower>,
    nodes: RefCell<Arena<'i, NodeImageSlot<'i>>>,
    relationships: RefCell<Arena<'i, RelationshipImageSlot<'i>>>,
}

impl<'i> StatementImages<'i> {
    /// Reserves room for at most `capacity` node images and `capacity`
    /// relationship images. The admitted document interpretation is the only
    /// one a node vector may be staged under.
    pub fn new(
        memory: &'i WriteMemory<'i>,
        document: Option<&'i EmbeddingTower>,
        capacity: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        if capacity > memory.limits.changes {
            return Err(StageError::Limit);
        }
        control(WritePhase::Allocate)?;
        Ok(Self {
            memory,
            document,
            nodes: RefCell::new(Arena::new(memory, capacity, control)?),
            relationships: RefCell::new(Arena::new(memory, capacity, control)?),
        })
    }

    /// Reserves every buffer one node image needs, sized from `budget`.
    pub fn node<'w>(
        &'w self,
        budget: NodeImageBudget,
        control: &mut WriteControl<'_>,
    ) -> Result<NodeImageBuilder<'w, 'i>, StageError> {
        let text = budget.text_bytes.unwrap_or(0);
        let dims = match budget.vector_dims {
            Some(dims) => {
                if self.document.is_none() {
                    return Err(StageError::InvalidInput);
                }
                usize::try_from(dims).map_err(|_| StageError::Limit)?
            }
            None => 0,
        };
        Ok(NodeImageBuilder {
            images: self,
            slot: NodeImageSlot {
                values: PropertySlot::new(self.memory, budget.values(), control)?,
                labels: Arena::new(self.memory, budget.labels, control)?,
                label_bytes: Arena::new(self.memory, budget.label_bytes, control)?,
                text: Arena::new(self.memory, text, control)?,
                vector: Arena::new(self.memory, dims, control)?,
                contents: Arena::new(self.memory, 1, control)?,
            },
            budget,
            text: false,
        })
    }

    /// Reserves every buffer one relationship image needs.
    pub fn relationship<'w>(
        &'w self,
        budget: RelationshipImageBudget,
        control: &mut WriteControl<'_>,
    ) -> Result<RelationshipImageBuilder<'w, 'i>, StageError> {
        Ok(RelationshipImageBuilder {
            images: self,
            slot: RelationshipImageSlot {
                values: PropertySlot::new(self.memory, budget.values(), control)?,
                type_bytes: Arena::new(self.memory, budget.type_bytes, control)?,
            },
        })
    }
}

/// One node image under construction. Nothing is visible to the overlay until
/// `finish` succeeds; dropping the builder releases every buffer it reserved.
pub struct NodeImageBuilder<'w, 'i> {
    images: &'w StatementImages<'i>,
    slot: NodeImageSlot<'i>,
    budget: NodeImageBudget,
    text: bool,
}

impl<'w, 'i> NodeImageBuilder<'w, 'i> {
    /// Copies one label.
    pub fn label(
        &mut self,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        let name = GraphName::new(copy_str(
            &mut self.slot.label_bytes,
            name.as_str(),
            control,
        )?)
        .map_err(|_| StageError::InvalidInput)?;
        self.slot.labels.push(name)
    }

    /// Copies one complete property, preserving its exact typed value.
    pub fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        self.slot.values.property(name, value, control)
    }

    /// Copies the stored text, preserving present-empty text.
    pub fn text(&mut self, text: &str, control: &mut WriteControl<'_>) -> Result<(), StageError> {
        if self.text || self.budget.text_bytes != Some(text.len()) {
            return Err(StageError::InvalidInput);
        }
        copy_str(&mut self.slot.text, text, control)?;
        self.text = true;
        Ok(())
    }

    /// Appends original vector coordinates; a vector may arrive in chunks.
    pub fn vector(
        &mut self,
        coordinates: &[f32],
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        if self.budget.vector_dims.is_none() {
            return Err(StageError::InvalidInput);
        }
        copy_list(&mut self.slot.vector, coordinates, control)?;
        Ok(())
    }

    /// Validates and normalizes the complete image, then appends it to the
    /// statement arena. A full arena is a loud `StageError::Limit`.
    pub fn finish(
        mut self,
        control: &mut WriteControl<'_>,
    ) -> Result<&'w CanonicalContents<'w>, StageError> {
        control(WritePhase::Canonical)?;
        if self.budget.text_bytes.is_some() != self.text {
            return Err(StageError::InvalidInput);
        }
        let text = if self.text {
            let bytes: &[u8] = &self.slot.text;
            // SAFETY: the stored text was copied once into this image's own
            // buffer and is never written again.
            let bytes = unsafe { retained(bytes) };
            Some(std::str::from_utf8(bytes).map_err(|_| StageError::InvalidInput)?)
        } else {
            None
        };
        let embedding = match self.budget.vector_dims {
            Some(dims) => {
                if usize::try_from(dims).ok() != Some(self.slot.vector.len()) {
                    return Err(StageError::InvalidInput);
                }
                let document = self.images.document.ok_or(StageError::InvalidInput)?;
                let coordinates: &[f32] = &self.slot.vector;
                // SAFETY: the coordinates were copied once into this image's
                // own buffer and are never written again.
                let coordinates = unsafe { retained(coordinates) };
                Some(CanonicalEmbedding::new(document, coordinates)?)
            }
            None => None,
        };
        let labels: *mut [GraphName<'i>] = self.slot.labels.as_mut_slice();
        let properties: *mut [GraphProperty<'i>] = self.slot.values.properties.as_mut_slice();
        // SAFETY: these are the only references ever taken to the complete
        // label and property descriptor buffers of this image. Both buffers
        // are fixed-capacity and heap-stable. `CanonicalContents::node`
        // sorts them in place and then retains them only as shared slices;
        // neither buffer is touched through its `Arena` again, and the
        // resulting image is exposed only through a `&'w self` borrow.
        let (labels, properties) = unsafe { (&mut *labels, &mut *properties) };
        let contents = CanonicalContents::node(labels, properties, text, embedding)?;
        self.slot.contents.push(contents)?;
        let pointer: *const CanonicalContents<'i> =
            self.slot.contents.first().ok_or(StageError::InvalidInput)?;
        // A full arena refuses here: the slot drops with every byte it
        // reserved, and no reference to it has escaped.
        self.images
            .nodes
            .try_borrow_mut()
            .map_err(|_| StageError::InvalidInput)?
            .push(self.slot)?;
        // SAFETY: `pointer` names the single element of this image's own
        // one-element buffer. Moving the slot into the outer array moved only
        // that buffer's header, and the outer array now owns it for the whole
        // life of `self.images`, which `'w` borrows.
        Ok(unsafe { &*pointer })
    }
}

/// One relationship image under construction.
pub struct RelationshipImageBuilder<'w, 'i> {
    images: &'w StatementImages<'i>,
    slot: RelationshipImageSlot<'i>,
}

impl<'w, 'i> RelationshipImageBuilder<'w, 'i> {
    /// Copies one complete property, preserving its exact typed value.
    pub fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        self.slot.values.property(name, value, control)
    }

    /// Appends the complete image to the statement arena. Endpoints and type
    /// are the relationship's immutable topology, so they are supplied here.
    pub fn finish(
        mut self,
        source: NodeRef<'static>,
        target: NodeRef<'static>,
        relationship_type: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<WriteImage<'w, 'static>, StageError> {
        control(WritePhase::Canonical)?;
        let relationship_type = GraphName::new(copy_str(
            &mut self.slot.type_bytes,
            relationship_type.as_str(),
            control,
        )?)
        .map_err(|_| StageError::InvalidInput)?;
        let properties: &[GraphProperty<'i>] = &self.slot.values.properties;
        // SAFETY: the property descriptors are complete and never written
        // again; the buffer is heap-stable and exposed only through `'w`.
        let properties = unsafe { retained(properties) };
        self.images
            .relationships
            .try_borrow_mut()
            .map_err(|_| StageError::InvalidInput)?
            .push(self.slot)?;
        Ok(WriteImage::Relationship {
            source,
            target,
            relationship_type,
            properties,
        })
    }
}

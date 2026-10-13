//! std/hashbrown 0.16 allocation layout, pinned by Rust 1.93 allocator tests.

/// Requested bytes for a fresh table (or its full retained capacity).
/// Includes empty buckets, alignment padding and trailing control tags.
pub(crate) fn allocation_bytes<T>(capacity: usize) -> Option<usize> {
    if capacity == 0 {
        return Some(0);
    }
    // hashbrown uses 16-byte SSE2 groups and 8-byte NEON/generic groups.
    let group = if cfg!(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2",
        not(miri)
    )) {
        16
    } else {
        std::mem::size_of::<usize>()
    };
    let size = std::mem::size_of::<T>();
    let align = std::mem::align_of::<T>().max(group);
    let buckets = if capacity < 15 {
        let minimum = match (group, size) {
            (16, 0..=1) => 14,
            (16, 2..=3) | (8, 0..=1) => 7,
            _ => 3,
        };
        capacity
            .max(minimum)
            .checked_add(1)?
            .checked_next_power_of_two()?
    } else {
        (capacity.checked_mul(8)? / 7).checked_next_power_of_two()?
    };
    let storage = size.checked_mul(buckets)?.checked_add(align - 1)? & !(align - 1);
    let bytes = storage.checked_add(buckets)?.checked_add(group)?;
    (bytes <= (isize::MAX as usize).checked_sub(align - 1)?).then_some(bytes)
}

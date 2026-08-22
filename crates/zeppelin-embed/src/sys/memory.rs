//! Portable Unix virtual-memory residency accounting.

use std::io;

/// Counts the byte intersections belonging to resident pages of a valid range.
pub(crate) fn mincore_resident_bytes(range: &[u8]) -> io::Result<u64> {
    if range.is_empty() {
        return Ok(0);
    }
    let page_size_raw = unsafe {
        // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
        libc::sysconf(libc::_SC_PAGESIZE)
    };
    let page_size = usize::try_from(page_size_raw)
        .ok()
        .filter(|size| *size != 0)
        .ok_or_else(|| io::Error::other("sysconf returned an invalid page size"))?;
    let start = range.as_ptr() as usize;
    let end = start
        .checked_add(range.len())
        .ok_or_else(|| io::Error::other("mapped range end overflow"))?;
    let aligned_start = start / page_size * page_size;
    let aligned_end = end
        .checked_add(page_size.saturating_sub(1))
        .ok_or_else(|| io::Error::other("mapped range alignment overflow"))?
        / page_size
        * page_size;
    let page_count = aligned_end
        .checked_sub(aligned_start)
        .ok_or_else(|| io::Error::other("mapped range underflow"))?
        / page_size;
    const STATUS_CAPACITY: usize = 1_024;
    let mut statuses = [0 as libc::c_char; STATUS_CAPACITY];
    let mut first_page = 0_usize;
    let mut resident = 0_u64;
    while first_page < page_count {
        let pages = STATUS_CAPACITY.min(page_count.saturating_sub(first_page));
        let byte_offset = first_page
            .checked_mul(page_size)
            .ok_or_else(|| io::Error::other("mincore byte offset overflow"))?;
        let chunk_start = aligned_start
            .checked_add(byte_offset)
            .ok_or_else(|| io::Error::other("mincore address overflow"))?;
        let chunk_length = pages
            .checked_mul(page_size)
            .ok_or_else(|| io::Error::other("mincore chunk length overflow"))?;
        let result = unsafe {
            // SAFETY: the queried pages are the outward page-aligned extent of
            // `range`, and `statuses` has one byte for every page in this chunk.
            libc::mincore(
                chunk_start as *mut libc::c_void,
                chunk_length,
                statuses.as_mut_ptr(),
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        for (index, status) in statuses.iter().take(pages).enumerate() {
            if *status & 1 != 1 {
                continue;
            }
            let page_offset = index
                .checked_mul(page_size)
                .and_then(|offset| chunk_start.checked_add(offset))
                .ok_or_else(|| io::Error::other("resident page address overflow"))?;
            let page_end = page_offset
                .checked_add(page_size)
                .ok_or_else(|| io::Error::other("resident page end overflow"))?;
            let intersection_start = page_offset.max(start);
            let intersection_end = page_end.min(end);
            let intersection = intersection_end.saturating_sub(intersection_start);
            resident = resident
                .checked_add(
                    u64::try_from(intersection)
                        .map_err(|_| io::Error::other("resident byte count exceeds u64"))?,
                )
                .ok_or_else(|| io::Error::other("resident byte count overflow"))?;
        }
        first_page = first_page.saturating_add(pages);
    }
    Ok(resident)
}

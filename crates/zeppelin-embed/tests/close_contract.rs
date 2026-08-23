#![allow(clippy::expect_used)]

mod lifecycle_support;

use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use lifecycle_support::{published_store, test_guard};
use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, Store, StoreError, StoreState,
};
use zeppelin_embed::scan::{F32Rows, ScanOptions, ScanQuery, ScanRequest, ScanRows};

#[test]
fn close_releases_file_locks_and_reopen_succeeds() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open writer");

    store.close().expect("close writer");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen writer");
    reopened.close().expect("close reopened writer");
}

#[test]
fn close_unmaps_all_segments() {
    let _guard = test_guard();
    let fixture = published_store(41);
    let store = Store::open(fixture.path(), OpenOptions::default()).expect("open mapped store");
    let snapshot = store.snapshot().expect("published snapshot");
    let segment = snapshot.segments().first().expect("published segment");
    let codes = segment.bit4_codes().expect("mapped vector codes");
    let mapped_address = codes.as_ptr() as usize;
    let expected_mapped_bytes =
        std::fs::metadata(fixture.path().join(segment.meta().id.file_name()))
            .expect("segment metadata")
            .len();
    let live_stats = store.stats().expect("live mapping stats");
    assert_eq!(live_stats.mapped_bytes, expected_mapped_bytes);
    assert_eq!(live_stats.mapped_bytes, live_stats.segment_bytes);
    assert!(live_stats.mapped_resident_bytes > 0);
    assert!(live_stats.mapped_resident_bytes <= live_stats.mapped_bytes);
    assert!(is_address_mapped(mapped_address).expect("query live mapping"));
    drop(snapshot);

    store.close().expect("close mapped store");

    assert!(matches!(store.stats(), Err(StoreError::Closed)));
    assert!(
        !is_address_mapped(mapped_address).expect("query released mapping"),
        "the segment page remains in the process mapping table after close"
    );
}

#[test]
fn close_is_idempotent() {
    let _guard = test_guard();
    const CALLERS: usize = 8;

    let directory = tempdir().expect("store directory");
    let store = Arc::new(Store::open(directory.path(), OpenOptions::default()).expect("open"));
    let admitted_reader = store.snapshot().expect("admitted reader");
    let start = Arc::new(Barrier::new(CALLERS + 1));
    let (returned_tx, returned_rx) = mpsc::channel();
    let mut callers = Vec::new();
    for _ in 0..CALLERS {
        let caller_store = Arc::clone(&store);
        let caller_start = Arc::clone(&start);
        let caller_returned = returned_tx.clone();
        callers.push(std::thread::spawn(move || {
            caller_start.wait();
            let result = caller_store.close();
            caller_returned.send(result).expect("report close result");
        }));
    }
    drop(returned_tx);
    start.wait();
    while store.state().expect("lifecycle state") == StoreState::Open {
        std::thread::yield_now();
    }
    assert!(
        returned_rx.recv_timeout(Duration::from_millis(25)).is_err(),
        "a concurrent close returned before the admitted reader drained"
    );

    drop(admitted_reader);
    for _ in 0..CALLERS {
        returned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("close caller returned")
            .expect("close caller succeeded");
    }
    for caller in callers {
        caller.join().expect("close caller thread");
    }
    store.close().expect("sequential repeat close");
    assert_eq!(store.state().expect("closed state"), StoreState::Closed);

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen");
    reopened.close().expect("close reopen");
}

#[test]
fn calls_after_close_return_typed_closed_error() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store.close().expect("close");

    // `open` constructs a new handle, `state` observes lifecycle, and `close`
    // is explicitly idempotent. Every operation admitted only while open is
    // enumerated here as the surface grows.
    assert!(matches!(store.snapshot(), Err(StoreError::Closed)));
    assert!(matches!(store.stats(), Err(StoreError::Closed)));
    assert!(matches!(
        store.ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0_f32],
        )])),
        Err(zeppelin_embed::ingest::IngestError::Store(
            StoreError::Closed
        ))
    ));
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32]);
    assert!(matches!(
        store.top_k_with_options(
            ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&rows),
                row_mask: None,
            },
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        ),
        Err(zeppelin_embed::lifecycle::QueryError::Store(
            StoreError::Closed
        ))
    ));
    assert!(matches!(
        store.search(
            SearchRequest::new(&query),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        ),
        Err(zeppelin_embed::lifecycle::QueryError::Store(
            StoreError::Closed
        ))
    ));
    assert_eq!(store.state().expect("closed state"), StoreState::Closed);
    store.close().expect("idempotent close remains valid");
}

#[test]
fn close_during_inflight_search_drains_or_cancels_deterministically() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let options = OpenOptions::new().with_reader_drain_timeout(Duration::ZERO);
    let store = Arc::new(Store::open(directory.path(), options).expect("open"));
    let (admitted_tx, admitted_rx) = mpsc::sync_channel(0);
    let query_store = Arc::clone(&store);
    let query = std::thread::spawn(move || {
        let snapshot = query_store.snapshot().expect("admit query snapshot");
        snapshot.check_active().expect("query starts active");
        admitted_tx.send(()).expect("report admitted query");
        snapshot
            .wait_for_close_cancellation()
            .expect("wait for close cancellation");
        snapshot.check_active()
    });
    admitted_rx.recv().expect("query admitted");

    store.close().expect("close cancels and drains query");

    assert!(matches!(
        query.join().expect("query thread"),
        Err(StoreError::ReadCancelled)
    ));
    assert_eq!(store.state().expect("closed state"), StoreState::Closed);
}

#[test]
fn no_background_thread_survives_close() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let before = os_thread_ids().expect("census before open");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let capacity = start_query_pool(&store);
    let during = os_thread_ids().expect("census after open");
    let spawned = during.difference(&before).copied().collect::<Vec<_>>();
    assert!(
        spawned.len() >= capacity.saturating_add(1),
        "kernel census found {spawned:?}, fewer than {capacity} pool workers plus lifecycle thread"
    );

    store.close().expect("close");

    let after = os_thread_ids().expect("census after close");
    assert!(
        spawned.iter().all(|thread| !after.contains(thread)),
        "kernel thread census still contains store threads after close: {spawned:?}"
    );
}

#[test]
fn drop_without_close_best_effort_releases() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let before = os_thread_ids().expect("census before open");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let capacity = start_query_pool(&store);
    let during = os_thread_ids().expect("census after open");
    let spawned = during.difference(&before).copied().collect::<Vec<_>>();
    assert!(
        spawned.len() >= capacity.saturating_add(1),
        "writer lifecycle and query pool threads exist"
    );

    drop(store);

    let after = os_thread_ids().expect("census after drop");
    assert!(
        spawned.iter().all(|thread| !after.contains(thread)),
        "kernel thread census still contains store threads after drop: {spawned:?}"
    );
    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen after drop");
    let reopened_stats = reopened
        .stats()
        .expect("stats after pool-owning store drop");
    assert_eq!(reopened_stats.query_pool_bytes, 0);
    assert_eq!(reopened_stats.resident_owned_bytes, 0);
    reopened.close().expect("close reopened store");
}

fn start_query_pool(store: &Store) -> usize {
    let capacity = zeppelin_embed::scan::physical_thread_capacity().expect("worker capacity");
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32; capacity.max(2) * 64]);
    let outcome = store
        .top_k_with_options(
            ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&rows),
                row_mask: None,
            },
            1,
            ScanOptions {
                thread_budget: capacity,
            },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("start and use persistent query pool");
    assert_eq!(outcome.stats.worker_thread_ids.len(), capacity);
    capacity
}

#[test]
fn snapshot_lease_outliving_store_observes_read_cancelled() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let lease = store.snapshot().expect("admitted lease");
    lease.check_active().expect("lease starts active");

    drop(store);

    assert!(matches!(
        lease.check_active(),
        Err(StoreError::ReadCancelled)
    ));
}

#[cfg(target_os = "macos")]
fn os_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    let task = unsafe {
        // SAFETY: libSystem initializes the current-task port before Rust `main`.
        mach_task_self_
    };
    let mut threads = std::ptr::null_mut();
    let mut count = 0_u32;
    let result = unsafe {
        // SAFETY: the two out pointers are valid writable storage for Mach's allocated array.
        libc::task_threads(task, &raw mut threads, &raw mut count)
    };
    if result != libc::KERN_SUCCESS {
        return Err(std::io::Error::other(format!(
            "task_threads failed with Mach code {result}"
        )));
    }
    let count_usize =
        usize::try_from(count).map_err(|_| std::io::Error::other("thread count exceeds usize"))?;
    let ports = unsafe {
        // SAFETY: successful `task_threads` returned `count` initialized port names.
        std::slice::from_raw_parts(threads, count_usize)
    };
    let ids = ports.iter().map(|port| u64::from(*port)).collect();
    for port in ports {
        let _ = unsafe {
            // SAFETY: each name is a send right returned by `task_threads` to this task.
            mach_port_deallocate(task, *port)
        };
    }
    let bytes = count_usize
        .checked_mul(std::mem::size_of::<libc::thread_t>())
        .ok_or_else(|| std::io::Error::other("thread array byte length overflow"))?;
    let address = threads as libc::vm_address_t;
    let size = libc::vm_size_t::try_from(bytes)
        .map_err(|_| std::io::Error::other("thread array byte length exceeds vm_size_t"))?;
    let release = unsafe {
        // SAFETY: this is the exact task-allocated array returned by `task_threads`.
        libc::vm_deallocate(task, address, size)
    };
    if release != libc::KERN_SUCCESS {
        return Err(std::io::Error::other(format!(
            "vm_deallocate failed with Mach code {release}"
        )));
    }
    Ok(ids)
}

#[cfg(target_os = "linux")]
fn os_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    std::fs::read_dir("/proc/self/task")?
        .map(|entry| {
            let entry = entry?;
            entry
                .file_name()
                .to_string_lossy()
                .parse::<u64>()
                .map_err(std::io::Error::other)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn is_address_mapped(address: usize) -> std::io::Result<bool> {
    const VM_REGION_BASIC_INFO_64: libc::c_int = 9;
    const VM_REGION_BASIC_INFO_COUNT_64: libc::mach_msg_type_number_t = 9;
    #[repr(C, packed(4))]
    #[derive(Default)]
    struct VmRegionBasicInfo64 {
        protection: libc::vm_prot_t,
        max_protection: libc::vm_prot_t,
        inheritance: libc::vm_inherit_t,
        shared: libc::boolean_t,
        reserved: libc::boolean_t,
        offset: libc::memory_object_offset_t,
        behavior: libc::c_int,
        user_wired_count: libc::c_ushort,
    }
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_vm_region(
            target_task: libc::vm_map_t,
            address: *mut libc::mach_vm_address_t,
            size: *mut libc::mach_vm_size_t,
            flavor: libc::c_int,
            info: *mut libc::c_int,
            info_count: *mut libc::mach_msg_type_number_t,
            object_name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    if std::mem::size_of::<VmRegionBasicInfo64>() != 36
        || std::mem::offset_of!(VmRegionBasicInfo64, offset) != 20
        || std::mem::offset_of!(VmRegionBasicInfo64, behavior) != 28
        || std::mem::offset_of!(VmRegionBasicInfo64, user_wired_count) != 32
    {
        return Err(std::io::Error::other(
            "vm_region_basic_info_64 Rust layout does not match Darwin pack(4)",
        ));
    }
    let target = u64::try_from(address)
        .map_err(|_| std::io::Error::other("mapping address exceeds mach_vm_address_t"))?;
    let mut region_address = target;
    let mut region_size = 0_u64;
    let mut info = VmRegionBasicInfo64::default();
    let mut info_count = VM_REGION_BASIC_INFO_COUNT_64;
    let mut object_name = 0;
    let task = unsafe {
        // SAFETY: libSystem initializes the current-task port before Rust `main`.
        mach_task_self_
    };
    let result = unsafe {
        // SAFETY: every out pointer refers to writable storage matching Darwin's
        // pack(4), 36-byte `vm_region_basic_info_64` layout and count 9.
        mach_vm_region(
            task,
            &raw mut region_address,
            &raw mut region_size,
            VM_REGION_BASIC_INFO_64,
            (&raw mut info).cast::<libc::c_int>(),
            &raw mut info_count,
            &raw mut object_name,
        )
    };
    if object_name != 0 {
        let _ = unsafe {
            // SAFETY: `mach_vm_region` returned this send right to the current task.
            mach_port_deallocate(task, object_name)
        };
    }
    if result != libc::KERN_SUCCESS {
        return Ok(false);
    }
    Ok(region_address <= target && target < region_address.saturating_add(region_size))
}

#[cfg(not(target_os = "macos"))]
fn is_address_mapped(address: usize) -> std::io::Result<bool> {
    let page_size = unsafe {
        // SAFETY: `_SC_PAGESIZE` takes no pointer argument and returns a process constant.
        libc::sysconf(libc::_SC_PAGESIZE)
    };
    let page_size = usize::try_from(page_size)
        .map_err(|_| std::io::Error::other("sysconf returned an invalid page size"))?;
    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(std::io::Error::other(
            "sysconf returned a malformed page size",
        ));
    }
    let page = address & !(page_size - 1);
    let mut residency = 0_u8;
    let result = unsafe {
        // SAFETY: `page` is aligned to the kernel page size. `mincore` only inspects the
        // supplied address range and writes exactly one residency byte for one page.
        libc::mincore(
            page as *mut libc::c_void,
            page_size,
            (&raw mut residency).cast::<libc::c_char>(),
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOMEM) {
        Ok(false)
    } else {
        Err(error)
    }
}

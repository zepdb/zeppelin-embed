mod common;
use std::mem::size_of;
use zeppelin_embed_ffi::*;

#[test]
fn live_batch_preserves_handles_and_rejects_closed_participants() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_str().unwrap().as_bytes();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let mut handles = [0; 2];
    let names: [&[u8]; 2] = [b"a", b"b"];
    for (i, name) in names.iter().enumerate() {
        let mut open: ZeOpenRequest = common::sized_zeroed();
        open.commit_tier = 1;
        open.max_resident_bytes = u64::MAX;
        open.max_temp_bytes = u64::MAX;
        let request = ZeNamespaceOpenRequest {
            abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
            abi_reserved: 0,
            root: path.as_ptr(),
            root_len: path.len(),
            name: name.as_ptr(),
            name_len: name.len(),
            open,
            spec: &spec,
        };
        assert_eq!(
            ze_namespace_open(&request, &mut handles[i]),
            ZeErrorCode::ZeOk
        );
        assert_eq!(common::ingest_rows(handles[i], 1, 4), ZeErrorCode::ZeOk);
    }
    let deleted = ZeDocId { high: 0, low: 1 };
    let participants: Vec<_> = names
        .iter()
        .map(|name| {
            let mut p: ZeNamespaceMutation = common::sized_zeroed();
            p.name = name.as_ptr();
            p.name_len = name.len();
            p.spec = &spec;
            p.upserts = common::sized_zeroed();
            p.upserts.batch = common::sized_zeroed();
            p.deletes = &deleted;
            p.delete_count = 1;
            p
        })
        .collect();
    let mut generations = [u64::MAX; 2];
    let mut batch: ZeNamespaceBatchRequest = common::sized_zeroed();
    batch.root = path.as_ptr();
    batch.root_len = path.len();
    batch.participants = participants.as_ptr();
    batch.participant_count = 2;
    batch.generations = generations.as_mut_ptr();
    let request = ZeNamespaceBatchLiveRequest {
        abi_size: size_of::<ZeNamespaceBatchLiveRequest>() as u32,
        abi_reserved: 0,
        batch,
        handles: handles.as_ptr(),
    };
    assert_eq!(ze_namespace_batch(&batch), ZeErrorCode::ZeErrStoreBusy);
    // A separately owned root writer excludes coordinator admission.
    let (code, coordinator) = common::open_path(root.path());
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(
        ze_namespace_batch_live(&request),
        ZeErrorCode::ZeErrStoreBusy
    );
    assert_eq!(generations, [u64::MAX; 2]);
    assert_eq!(ze_close(coordinator), ZeErrorCode::ZeOk);
    #[cfg(feature = "abi-panic-probe")]
    if std::env::var_os("ZE_LIVE_BATCH_INTERRUPT_CHILD").is_some() {
        arm_namespace_batch_step_probe("live states installed");
        assert_eq!(ze_namespace_batch_live(&request), ZeErrorCode::ZeErrIo);
        assert_eq!(generations, [u64::MAX; 2]);
        for handle in handles {
            assert_ne!(common::ingest_rows(handle, 2, 4), ZeErrorCode::ZeOk);
            assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
        }
        // The root rename committed; reopening resolves it independently.
        for name in names {
            let mut open: ZeOpenRequest = common::sized_zeroed();
            open.commit_tier = 1;
            open.max_resident_bytes = u64::MAX;
            open.max_temp_bytes = u64::MAX;
            let reopen = ZeNamespaceOpenRequest {
                abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
                abi_reserved: 0,
                root: path.as_ptr(),
                root_len: path.len(),
                name: name.as_ptr(),
                name_len: name.len(),
                open,
                spec: &spec,
            };
            let mut handle = 0;
            assert_eq!(ze_namespace_open(&reopen, &mut handle), ZeErrorCode::ZeOk);
            let mut state: ZeStateReport = common::sized_zeroed();
            assert_eq!(ze_state(handle, &mut state), ZeErrorCode::ZeOk);
            let vector = [0.25, 0.5, 0.75, 1.0];
            let search = common::valid_search_request(&vector);
            let mut result: ZeSearchResult = common::sized_zeroed();
            assert_eq!(ze_search(handle, &search, &mut result), ZeErrorCode::ZeOk);
            assert_eq!(result.generation, 3);
            assert_eq!(result.hit_count, 0);
            assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
            assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
        }
        return;
    }
    assert_eq!(ze_namespace_batch_live(&request), ZeErrorCode::ZeOk);
    assert!(generations.iter().all(|g| *g != u64::MAX));
    for handle in handles {
        assert_eq!(common::ingest_rows(handle, 2, 4), ZeErrorCode::ZeOk);
    }
    #[cfg(feature = "abi-panic-probe")]
    if std::env::var_os("ZE_LIVE_BATCH_POISON_CHILD").is_some() {
        arm_abi_panic_probe("ze_namespace_batch_live");
        assert_eq!(ze_namespace_batch_live(&request), ZeErrorCode::ZeErrPanic);
        assert_eq!(
            ze_namespace_batch_live(&request),
            ZeErrorCode::ZeErrPoisoned
        );
        for handle in handles {
            let mut state: ZeStateReport = common::sized_zeroed();
            assert_eq!(ze_state(handle, &mut state), ZeErrorCode::ZeErrPoisoned);
            assert_eq!(ze_close(handle), ZeErrorCode::ZeErrPoisoned);
        }
        return;
    }
    assert_eq!(ze_close(handles[1]), ZeErrorCode::ZeOk);
    generations.fill(u64::MAX);
    assert_eq!(ze_namespace_batch_live(&request), ZeErrorCode::ZeErrClosed);
    assert_eq!(generations, [u64::MAX; 2]);
    assert_eq!(ze_close(handles[0]), ZeErrorCode::ZeOk);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn panic_poisons_every_live_participant() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "live_batch_preserves_handles_and_rejects_closed_participants",
            "--nocapture",
        ])
        .env("ZE_LIVE_BATCH_POISON_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn indeterminate_commit_leaves_outputs_untouched_and_fences_writes() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "live_batch_preserves_handles_and_rejects_closed_participants",
            "--nocapture",
        ])
        .env("ZE_LIVE_BATCH_INTERRUPT_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

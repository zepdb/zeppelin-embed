//! Exploratory matrices. These do not assert a contract; they print what the
//! kernel actually does so the protocol can be designed on measurements.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::path::Path;

use windows_storage_probe::win32;
use windows_storage_probe::Scratch;

fn code(result: &io::Result<()>) -> String {
    match result {
        Ok(()) => "OK".to_owned(),
        Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
    }
}

fn share_name(mask: win32::DWORD) -> String {
    let mut parts = Vec::new();
    if mask & win32::FILE_SHARE_READ != 0 {
        parts.push("READ");
    }
    if mask & win32::FILE_SHARE_WRITE != 0 {
        parts.push("WRITE");
    }
    if mask & win32::FILE_SHARE_DELETE != 0 {
        parts.push("DELETE");
    }
    if parts.is_empty() {
        "NONE".to_owned()
    } else {
        parts.join("|")
    }
}

/// Which sharing masks on an open reader permit a replacing rename of the file
/// it has open, and under which `MoveFileEx` flags.
#[test]
fn replacement_matrix_over_an_open_reader() {
    let scratch = Scratch::new("diag-replace").expect("scratch");
    let shares = [
        0,
        win32::FILE_SHARE_READ,
        win32::FILE_SHARE_DELETE,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_DELETE,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
    ];
    // `MoveFileExW` with explicit flags, and `std::fs::rename`, which Rust
    // implements with POSIX-semantics `FileRenameInformationEx` before it falls
    // back to `MoveFileExW`.
    let movers: [(&str, fn(&Path, &Path) -> io::Result<()>); 3] = [
        ("MoveFileEx-REPLACE", |from, to| {
            win32::move_file_ex(from, to, win32::MOVEFILE_REPLACE_EXISTING)
        }),
        ("MoveFileEx-REPL+WT", |from, to| {
            win32::move_file_ex(
                from,
                to,
                win32::MOVEFILE_REPLACE_EXISTING | win32::MOVEFILE_WRITE_THROUGH,
            )
        }),
        ("std-fs-rename", |from, to| std::fs::rename(from, to)),
    ];

    println!("MATRIX replacement-over-open-reader");
    for (index, share) in shares.iter().enumerate() {
        for (mover_index, (mover_label, mover)) in movers.iter().enumerate() {
            let target = scratch
                .path()
                .join(format!("target-{index}-{mover_index}.bin"));
            let source = scratch
                .path()
                .join(format!("source-{index}-{mover_index}.bin"));
            std::fs::write(&target, b"old").expect("seed target");
            std::fs::write(&source, b"new").expect("seed source");

            let reader = win32::create_file(
                &target,
                win32::GENERIC_READ,
                *share,
                win32::OPEN_EXISTING,
                win32::FILE_ATTRIBUTE_NORMAL,
            );
            let reader_state = match &reader {
                Ok(_) => "open".to_owned(),
                Err(error) => format!("noopen{}", error.raw_os_error().unwrap_or(-1)),
            };
            let outcome = mover(&source, &target);
            let published = std::fs::read(&target).unwrap_or_default();
            println!(
                "  reader_share={:<22} mover={:<20} reader={:<8} result={:<8} target_now={:?}",
                share_name(*share),
                mover_label,
                reader_state,
                code(&outcome),
                String::from_utf8_lossy(&published)
            );
            drop(reader);
        }
    }
}

/// Can a file that a reader has **mapped** be replaced by a POSIX-semantics
/// rename, and does the old reader keep its validated bytes if so?
#[test]
fn posix_rename_over_a_mapped_reader() {
    let scratch = Scratch::new("diag-posix-mapped").expect("scratch");
    let target = scratch.path().join("mapped-target.zseg");
    let source = scratch.path().join("mapped-source.zseg");
    std::fs::write(&target, b"sealed-old").expect("seed");
    std::fs::write(&source, b"sealed-new").expect("seed");

    let map = win32::ReadOnlyMap::open(
        &target,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
    )
    .expect("map");
    let outcome = std::fs::rename(&source, &target);
    println!(
        "OBSERVE posix_mapped: std::fs::rename over a mapped reader -> {} mapped_bytes={:?} target_now={:?}",
        match &outcome {
            Ok(()) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        },
        String::from_utf8_lossy(map.as_bytes()),
        String::from_utf8_lossy(&std::fs::read(&target).unwrap_or_default())
    );
    drop(map);
}

/// The complete candidate protocol end to end, including the directory flush.
#[test]
fn full_protocol_including_directory_flush() {
    let scratch = Scratch::new("diag-full").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    let temporary = scratch.path().join("manifest.zman.tmp");

    let handle = win32::create_file(
        &temporary,
        win32::GENERIC_READ | win32::GENERIC_WRITE,
        0,
        win32::CREATE_ALWAYS,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("temp");
    let write = win32::write_all(&handle, b"generation-1");
    let data_flush = win32::flush(&handle);
    drop(handle);
    let renamed = std::fs::rename(&temporary, &manifest);
    let directory = win32::open_directory(scratch.path(), win32::GENERIC_WRITE);
    let directory_flush = match &directory {
        Ok(handle) => win32::flush(handle),
        Err(_) => Err(io::Error::other("directory handle unavailable")),
    };
    println!(
        "OBSERVE full_protocol: write={} data_flush={} rename={} dir_open={} dir_flush={}",
        code(&write),
        code(&data_flush),
        match &renamed {
            Ok(()) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        },
        match &directory {
            Ok(_) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        },
        code(&directory_flush)
    );
    println!(
        "OBSERVE full_protocol: published bytes={:?}",
        String::from_utf8_lossy(&std::fs::read(&manifest).unwrap_or_default())
    );
}

/// What a mapped reader permits: replacement, and deletion, as a function of
/// the share mask the mapping's file handle was opened with.
#[test]
fn mapped_reader_matrix() {
    let scratch = Scratch::new("diag-mapped").expect("scratch");
    let shares = [
        win32::FILE_SHARE_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_DELETE,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
    ];

    println!("MATRIX mapped-reader");
    for (index, share) in shares.iter().enumerate() {
        let target = scratch.path().join(format!("mapped-{index}.zseg"));
        let source = scratch.path().join(format!("mapped-src-{index}.zseg"));
        std::fs::write(&target, b"sealed-old").expect("seed");
        std::fs::write(&source, b"sealed-new").expect("seed");

        let map = win32::ReadOnlyMap::open(&target, *share).expect("map");
        let replace = win32::move_file_ex(
            &source,
            &target,
            win32::MOVEFILE_REPLACE_EXISTING | win32::MOVEFILE_WRITE_THROUGH,
        );
        let delete = win32::delete_file(&target);
        let bytes_after = map.as_bytes().to_vec();
        let linked_after = target.try_exists().unwrap_or(false);
        println!(
            "  map_share={:<22} replace={:<10} delete={:<10} mapped_bytes={:?} still_linked={}",
            share_name(*share),
            code(&replace),
            code(&delete),
            String::from_utf8_lossy(&bytes_after),
            linked_after
        );
        drop(map);
        let linked_after_unmap = target.try_exists().unwrap_or(false);
        println!("    after unmap: still_linked={linked_after_unmap}");
    }
}

/// Every plausible directory-flush route, with its exact outcome.
#[test]
fn directory_flush_matrix() {
    let scratch = Scratch::new("diag-dirflush").expect("scratch");
    println!("MATRIX directory-flush");
    for (label, access) in [
        ("GENERIC_READ", win32::GENERIC_READ),
        ("GENERIC_WRITE", win32::GENERIC_WRITE),
        (
            "READ|WRITE",
            win32::GENERIC_READ | win32::GENERIC_WRITE,
        ),
        ("0 (metadata only)", 0),
    ] {
        match win32::open_directory(scratch.path(), access) {
            Ok(handle) => {
                let flush = win32::flush(&handle);
                println!("  access={label:<20} open=OK flush={}", code(&flush));
            }
            Err(error) => println!(
                "  access={label:<20} open=ERR {} flush=n/a",
                error.raw_os_error().unwrap_or(-1)
            ),
        }
    }
}

/// Does a mapped view keep serving its validated bytes after the file it maps
/// has been unlinked? This is the lifetime property the engine depends on when
/// compaction retires an artifact under a live reader.
#[test]
fn mapped_bytes_survive_unlinking() {
    let scratch = Scratch::new("diag-survive").expect("scratch");
    let segment = scratch.path().join("segment.zseg");
    let payload: Vec<u8> = (0..4096_u32).map(|value| (value % 251) as u8).collect();
    std::fs::write(&segment, &payload).expect("seed");

    let map = win32::ReadOnlyMap::open(
        &segment,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
    )
    .expect("map");
    let delete = win32::delete_file(&segment);
    let linked = segment.try_exists().unwrap_or(false);
    let matches_payload = map.as_bytes() == payload.as_slice();
    println!(
        "OBSERVE survive: delete={} still_linked={linked} mapped_bytes_intact={matches_payload}",
        code(&delete)
    );
    drop(map);
    println!(
        "OBSERVE survive: after unmap still_linked={}",
        segment.try_exists().unwrap_or(false)
    );
}

/// How `std::fs::rename` (which is `MoveFileExW(REPLACE_EXISTING)`) compares,
/// so the engine's existing `StdVfs::rename` behaviour is measured too.
#[test]
fn std_rename_over_an_open_reader() {
    let scratch = Scratch::new("diag-stdrename").expect("scratch");
    let target = scratch.path().join("target.bin");
    let source = scratch.path().join("source.bin");
    std::fs::write(&target, b"old").expect("seed");
    std::fs::write(&source, b"new").expect("seed");

    let reader = win32::create_file(
        &target,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader");
    let outcome = std::fs::rename(&source, &target);
    println!(
        "OBSERVE std_rename: over a FILE_SHARE_DELETE reader -> {}",
        match &outcome {
            Ok(()) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        }
    );
    drop(reader);

    // And with a `std::fs::File` reader, which is what the engine actually holds.
    let target2 = scratch.path().join("target2.bin");
    let source2 = scratch.path().join("source2.bin");
    std::fs::write(&target2, b"old").expect("seed");
    std::fs::write(&source2, b"new").expect("seed");
    let std_reader = std::fs::File::open(&target2).expect("std reader");
    let outcome2 = std::fs::rename(&source2, &target2);
    println!(
        "OBSERVE std_rename: over a std::fs::File reader -> {}",
        match &outcome2 {
            Ok(()) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        }
    );
    drop(std_reader);
}

/// Confirms which step of the publication actually returned the error, so a
/// failure is attributed to the rename rather than to the temporary's creation.
#[test]
fn attribute_the_replacement_failure_to_its_exact_call() {
    let scratch = Scratch::new("diag-attribute").expect("scratch");
    let target = scratch.path().join("manifest.zman");
    let temporary = scratch.path().join("manifest.zman.tmp-publish");
    std::fs::write(&target, b"old").expect("seed");

    let reader = win32::create_file(
        &target,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader");

    let handle = win32::create_file(
        &temporary,
        win32::GENERIC_READ | win32::GENERIC_WRITE,
        0,
        win32::CREATE_ALWAYS,
        win32::FILE_ATTRIBUTE_NORMAL,
    );
    println!(
        "OBSERVE attribute: create temporary -> {}",
        match &handle {
            Ok(_) => "OK".to_owned(),
            Err(error) => format!("ERR {}", error.raw_os_error().unwrap_or(-1)),
        }
    );
    if let Ok(handle) = handle {
        let write = win32::write_all(&handle, b"new");
        let flush = win32::flush(&handle);
        println!(
            "OBSERVE attribute: write={} flush={}",
            code(&write),
            code(&flush)
        );
        drop(handle);
    }
    let moved = win32::move_file_ex(
        &temporary,
        &target,
        win32::MOVEFILE_REPLACE_EXISTING | win32::MOVEFILE_WRITE_THROUGH,
    );
    println!("OBSERVE attribute: MoveFileExW -> {}", code(&moved));
    drop(reader);
    let after_close = win32::move_file_ex(
        &temporary,
        &target,
        win32::MOVEFILE_REPLACE_EXISTING | win32::MOVEFILE_WRITE_THROUGH,
    );
    println!(
        "OBSERVE attribute: MoveFileExW after the reader closed -> {}",
        code(&after_close)
    );
    let _ = Path::new(&temporary);
}

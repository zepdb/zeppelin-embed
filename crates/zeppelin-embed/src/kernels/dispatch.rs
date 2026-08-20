//! Runtime-only feature detection and once-per-process table selection.

use std::sync::OnceLock;

use super::{KernelArm, KernelFeatures, KernelInitError, KernelTable, scalar};

static FEATURES: OnceLock<KernelFeatures> = OnceLock::new();
static ACTIVE_TABLE: OnceLock<KernelTable> = OnceLock::new();

pub(super) fn features() -> KernelFeatures {
    *FEATURES.get_or_init(detect_features)
}

pub(super) fn table_for_arm(arm: KernelArm) -> Option<KernelTable> {
    let detected = features();
    match arm {
        KernelArm::Scalar => Some(scalar::table()),
        KernelArm::Neon => neon_table(detected),
        KernelArm::Avx2 => avx2_table(detected),
    }
}

pub(super) fn variant_tables() -> [Option<KernelTable>; 4] {
    let detected = features();
    [
        Some(scalar::table()),
        neon_widen_table(detected),
        neon_dotprod_table(detected),
        avx2_table(detected),
    ]
}

pub(super) fn initialize() -> Result<KernelArm, KernelInitError> {
    let requested = requested_arm()?;
    let table = match requested {
        Some(arm) => {
            table_for_arm(arm).ok_or(KernelInitError::UnsupportedArm { requested: arm })?
        }
        None => best_runtime_table(),
    };

    if let Some(selected) = ACTIVE_TABLE.get() {
        if selected.arm == table.arm {
            return Ok(selected.arm);
        }
        return Err(KernelInitError::AlreadyInitialized {
            selected: selected.arm,
            requested: table.arm,
        });
    }

    let _set_result = ACTIVE_TABLE.set(table);
    Ok(ACTIVE_TABLE
        .get()
        .map_or(table.arm, |selected| selected.arm))
}

pub(super) fn active_table() -> &'static KernelTable {
    ACTIVE_TABLE.get_or_init(best_runtime_table)
}

fn requested_arm() -> Result<Option<KernelArm>, KernelInitError> {
    match std::env::var("ZE_KERNEL") {
        Ok(value) => match value.as_str() {
            "scalar" => Ok(Some(KernelArm::Scalar)),
            "neon" => Ok(Some(KernelArm::Neon)),
            "avx2" => Ok(Some(KernelArm::Avx2)),
            _ => Err(KernelInitError::UnknownOverride { value }),
        },
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(KernelInitError::NonUnicodeOverride),
    }
}

fn best_runtime_table() -> KernelTable {
    table_for_arm(KernelArm::Neon)
        .or_else(|| table_for_arm(KernelArm::Avx2))
        .unwrap_or_else(scalar::table)
}

fn detect_features() -> KernelFeatures {
    #[allow(unused_mut)]
    let mut detected = KernelFeatures::default();

    #[cfg(target_arch = "aarch64")]
    {
        detected.neon = std::arch::is_aarch64_feature_detected!("neon");
        detected.dotprod = std::arch::is_aarch64_feature_detected!("dotprod");
        detected.fp16 = std::arch::is_aarch64_feature_detected!("fp16");
        detected.i8mm = std::arch::is_aarch64_feature_detected!("i8mm");
        #[cfg(target_os = "macos")]
        {
            detected.dotprod |=
                super::neon::darwin_optional_feature(b"hw.optional.arm.FEAT_DotProd\0");
            detected.i8mm |= super::neon::darwin_optional_feature(b"hw.optional.arm.FEAT_I8MM\0");
            detected.sme2 = super::neon::darwin_optional_feature(b"hw.optional.arm.FEAT_SME2\0");
        }
        #[cfg(target_os = "linux")]
        {
            detected.sme2 = linux_sme2_from_auxv();
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        detected.avx2 = std::arch::is_x86_feature_detected!("avx2");
        detected.popcnt = std::arch::is_x86_feature_detected!("popcnt");
    }

    detected
}

#[cfg(target_arch = "aarch64")]
fn neon_table(features: KernelFeatures) -> Option<KernelTable> {
    features.neon.then(|| super::neon::table(features))
}

#[cfg(not(target_arch = "aarch64"))]
fn neon_table(_features: KernelFeatures) -> Option<KernelTable> {
    None
}

#[cfg(target_arch = "aarch64")]
fn neon_widen_table(features: KernelFeatures) -> Option<KernelTable> {
    features.neon.then(|| super::neon::widen_table(features))
}

#[cfg(not(target_arch = "aarch64"))]
fn neon_widen_table(_features: KernelFeatures) -> Option<KernelTable> {
    None
}

#[cfg(target_arch = "aarch64")]
fn neon_dotprod_table(features: KernelFeatures) -> Option<KernelTable> {
    (features.neon && features.dotprod).then(|| super::neon::dotprod_table(features))
}

#[cfg(not(target_arch = "aarch64"))]
fn neon_dotprod_table(_features: KernelFeatures) -> Option<KernelTable> {
    None
}

#[cfg(target_arch = "x86_64")]
fn avx2_table(features: KernelFeatures) -> Option<KernelTable> {
    (features.avx2 && features.popcnt).then(super::avx2::table)
}

#[cfg(not(target_arch = "x86_64"))]
fn avx2_table(_features: KernelFeatures) -> Option<KernelTable> {
    None
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn linux_sme2_from_auxv() -> bool {
    const AT_HWCAP2: usize = 26;
    const HWCAP2_SME2: usize = 1_usize << 37;
    let Ok(auxv) = std::fs::read("/proc/self/auxv") else {
        return false;
    };
    let word_bytes = size_of::<usize>();
    auxv.chunks_exact(word_bytes.saturating_mul(2))
        .any(|entry| {
            let Some(tag_bytes) = entry.get(..word_bytes) else {
                return false;
            };
            let Some(value_bytes) = entry.get(word_bytes..) else {
                return false;
            };
            let Some(tag) = native_usize(tag_bytes) else {
                return false;
            };
            let Some(value) = native_usize(value_bytes) else {
                return false;
            };
            tag == AT_HWCAP2 && value & HWCAP2_SME2 != 0
        })
}

#[cfg(all(
    target_arch = "aarch64",
    target_os = "linux",
    target_pointer_width = "64"
))]
fn native_usize(bytes: &[u8]) -> Option<usize> {
    let native: [u8; 8] = bytes.try_into().ok()?;
    Some(usize::from_ne_bytes(native))
}

#[cfg(all(
    target_arch = "aarch64",
    target_os = "linux",
    target_pointer_width = "32"
))]
fn native_usize(bytes: &[u8]) -> Option<usize> {
    let native: [u8; 4] = bytes.try_into().ok()?;
    Some(usize::from_ne_bytes(native))
}

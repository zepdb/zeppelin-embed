//! Strict, provenance-bearing measurements for the task-27 B1 campaign.

use std::error::Error;

use zeppelin_embed_bench::frontier::attestation::{
    CampaignPreflightOutcome, FileAttestationSource, MachineStateProvenance,
    default_attestation_path, preflight_with_attestation,
};
use zeppelin_embed_bench::frontier::measure::{
    MeasurementConfig, StridedI8Workload, SyntheticI8Workload, SystemMachineProbe, Workload,
    WorkloadSampler, measure_source_with_provenance,
};
use zeppelin_embed_bench::frontier::roofline::{RooflineInput, RooflineModel};
use zeppelin_embed_bench::frontier::variants::{RegisteredVariant, VariantRegistry};

const SEED: u64 = 0x27_2026_0820;
const ROW_COUNT: usize = 100_000;
const DIMENSION: usize = 768;
const STRIDE: usize = 3;

fn main() {
    if let Err(error) = run() {
        eprintln!("b1-measure: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let root = repository_root();
    let attestation = FileAttestationSource::new(default_attestation_path(&root));
    let provenance = match preflight_with_attestation(&SystemMachineProbe, &attestation) {
        CampaignPreflightOutcome::Idle { reasons } => {
            for reason in reasons {
                println!("PREFLIGHT IDLE: {reason}");
            }
            println!("CAMPAIGN IDLE: fail-closed; zero timings produced");
            return Ok(());
        }
        CampaignPreflightOutcome::Ready { provenance, .. } => provenance,
    };
    println!("PREFLIGHT READY: {}", provenance_label(&provenance));
    println!("shape: rows={ROW_COUNT} dimension={DIMENSION} stride={STRIDE} seed={SEED}");
    let registry = VariantRegistry::from_kernel_knob_space()?;
    let model = RooflineModel::from_default_calibration()?;
    let filter = std::env::args().nth(1);
    for variant in registry.materialized().iter().filter(|variant| {
        filter
            .as_ref()
            .is_none_or(|needle| variant.point().stable_id().contains(needle))
    }) {
        measure_contiguous(variant, &model, &provenance)?;
        measure_strided(variant, &model, &provenance)?;
    }
    Ok(())
}

fn measure_contiguous(
    variant: &RegisteredVariant,
    model: &RooflineModel,
    provenance: &MachineStateProvenance,
) -> Result<(), Box<dyn Error>> {
    let mut workload =
        SyntheticI8Workload::new("synthetic-contiguous", ROW_COUNT, DIMENSION, SEED)?;
    measure_one(variant, &mut workload, model, provenance)
}

fn measure_strided(
    variant: &RegisteredVariant,
    model: &RooflineModel,
    provenance: &MachineStateProvenance,
) -> Result<(), Box<dyn Error>> {
    let mut workload =
        StridedI8Workload::new("synthetic-strided", ROW_COUNT, DIMENSION, STRIDE, SEED)?;
    measure_one(variant, &mut workload, model, provenance)
}

fn measure_one<W: Workload<Error = zeppelin_embed_bench::frontier::measure::WorkloadError>>(
    variant: &RegisteredVariant,
    workload: &mut W,
    model: &RooflineModel,
    provenance: &MachineStateProvenance,
) -> Result<(), Box<dyn Error>> {
    let observation = workload.execute(variant)?;
    if !observation.correct {
        return Err(format!("{} failed the scalar oracle", variant.point().stable_id()).into());
    }
    let descriptor = workload.descriptor().clone();
    let mut sampler = WorkloadSampler::new(workload, variant);
    let measurement = measure_source_with_provenance(
        &mut sampler,
        MeasurementConfig::strict(),
        provenance.clone(),
    )?;
    let score = model.score(RooflineInput {
        bytes_touched: descriptor.bytes_touched,
        operation_count: descriptor.operation_count,
        elapsed_seconds: measurement.min_of_medians_ns / 1_000_000_000.0,
        cores: 1,
        compute_tier: descriptor.compute_tier,
        binding: descriptor.binding,
    })?;
    let gigabytes_per_second = descriptor.bytes_touched as f64 / measurement.min_of_medians_ns;
    println!(
        "RESULT variant={} workload={} provisional={} min_ns={:.3} gbps={:.6} roofline_percent={:.6} medians_ns={:?} rsd_percent={:?} discarded={} checksum={} fingerprint={} provenance={}",
        variant.point().stable_id(),
        descriptor.name,
        descriptor.provisional,
        measurement.min_of_medians_ns,
        gigabytes_per_second,
        score.achieved_percent,
        measurement.accepted_run_medians_ns,
        measurement.accepted_run_rsd_percent,
        measurement.discarded_runs,
        observation.checksum,
        observation.access_fingerprint,
        provenance_label(&measurement.machine_state),
    );
    println!("DIAGNOSTIC {}", score.loud_message());
    Ok(())
}

fn provenance_label(provenance: &MachineStateProvenance) -> String {
    match provenance {
        MachineStateProvenance::DirectProbe => "direct-probe".to_owned(),
        MachineStateProvenance::OperatorAttestation {
            timestamp,
            machine_identifier,
        } => format!("operator-attestation:{timestamp}:{machine_identifier}"),
    }
}

fn repository_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("bench crate lives under repository/crates")
        .to_path_buf()
}

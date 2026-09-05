//! Bounded tokenizer-only benchmark; inputs are an offline JSON string array.
use std::hint::black_box;
use std::time::Instant;
use zeppelin_embed_text::bundle::Bundle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let bundle = Bundle::open(args.next().ok_or("bundle path required")?)?;
    let texts: Vec<String> = serde_json::from_slice(&std::fs::read(
        args.next().ok_or("input JSON path required")?,
    )?)?;
    let role = args.next().ok_or("query or document role required")?;
    let rounds: usize = args.next().ok_or("round count required")?.parse()?;
    if texts.is_empty() || rounds == 0 || !matches!(role.as_str(), "query" | "document") {
        return Err("nonempty inputs, positive rounds and a valid role are required".into());
    }
    let encode = |text: &String| match role.as_str() {
        "query" => bundle.tokenize_query(text),
        _ => bundle.tokenize_document_chunks(std::slice::from_ref(text)),
    };
    for text in texts.iter().cycle().take(100) {
        black_box(encode(text)?);
    }
    let mut samples = Vec::with_capacity(texts.len() * rounds);
    let mut tokens = 0_usize;
    let mut checksum = 0_u64;
    for _ in 0..rounds {
        for text in &texts {
            let started = Instant::now();
            let output = black_box(encode(black_box(text))?);
            samples.push(started.elapsed().as_secs_f64());
            tokens += output.token_ids.len();
            for id in output.token_ids {
                checksum = checksum.wrapping_add(id as u64);
            }
        }
    }
    let total: f64 = samples.iter().sum();
    samples.sort_by(f64::total_cmp);
    let percentile =
        |p: f64| samples[((samples.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    let bytes: usize = texts.iter().map(String::len).sum::<usize>() * rounds;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "kind": "tokenizer_only", "role": role, "inputs": texts.len(), "rounds": rounds,
            "calls": samples.len(), "tokens": tokens, "checksum": checksum,
            "input_bytes": bytes, "total_seconds": total, "calls_per_second": samples.len() as f64 / total,
            "input_bytes_per_second": bytes as f64 / total, "output_tokens_per_second": tokens as f64 / total,
            "p50_us": percentile(0.50) * 1e6, "p95_us": percentile(0.95) * 1e6,
            "boundary": "encode through token IDs and masks; bundle load, JSON, warmup and checksum outside"
        }))?
    );
    Ok(())
}

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph_profile.rs"]
mod graph_profile;
fn main() {
    if let Err(e) = run() {
        eprintln!("graph-profile: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut m = graph_profile::manifest();
    let mut cases = m["local"].as_array().unwrap().clone();
    cases.extend(graph_profile::original_cases());
    if args == ["describe"] {
        for case in &mut cases {
            case["parameter_cells"] = zeppelin_embed_bench::harness_json::json!(
                case["parameters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| (
                        p[0].as_str().unwrap().to_owned(),
                        graph_profile::primitive(&graph_profile::tck::parse_value(
                            p[1].as_str().unwrap()
                        ))
                    ))
                    .collect::<std::collections::BTreeMap<_, _>>()
            );
            if !case["error"].is_null() {
                use zeppelin_embed_ffi::ZeErrorCode as E;
                let code = match case["error"]["c"].as_str().unwrap() {
                    "ZeErrInvalidArgument" => E::ZeErrInvalidArgument,
                    "ZeErrQueryUnsupported" => E::ZeErrQueryUnsupported,
                    "ZeErrParameter" => E::ZeErrParameter,
                    "ZeErrEndpoint" => E::ZeErrEndpoint,
                    _ => return Err("unmapped declared error".into()),
                };
                case["error"]["c_code"] = zeppelin_embed_bench::harness_json::json!(code as i32);
            }

            case["expected_cells"] = zeppelin_embed_bench::harness_json::json!(
                case["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(
                            |v| graph_profile::primitive(&graph_profile::tck::parse_value(
                                v.as_str().unwrap()
                            ))
                        )
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            );
            let mut rows = case["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    format!(
                        "{:?}",
                        r.as_array()
                            .unwrap()
                            .iter()
                            .map(|v| {
                                let mut value =
                                    graph_profile::tck::parse_value(v.as_str().unwrap());
                                if case["mode"] == "bag-lists-unordered" {
                                    value.sort_lists();
                                }
                                value
                            })
                            .collect::<Vec<_>>()
                    )
                })
                .collect::<Vec<_>>();
            if !case["ordered"].as_bool().unwrap() {
                rows.sort();
            }
            case["expected_rows"] = zeppelin_embed_bench::harness_json::json!(rows);
        }
        m["cases"] = zeppelin_embed_bench::harness_json::json!(cases);
        println!("{m}");
        return Ok(());
    }
    if args.len() != 3 || args[0] != "run" {
        return Err("usage: graph-profile describe; graph-profile run CASE PATH".into());
    }
    let case = cases
        .iter()
        .find(|c| c["id"].as_str() == Some(args[1].as_str()))
        .ok_or("unknown case")?;
    if !["rust-cypher", "rust-structured", "c-cypher", "c-structured"].contains(&args[2].as_str()) {
        return Err(
            "unsupported Rust/C runner path; Swift paths use GraphProfileParityTests".into(),
        );
    }
    println!("{}", graph_profile::run_local(case, &args[2])?);
    Ok(())
}

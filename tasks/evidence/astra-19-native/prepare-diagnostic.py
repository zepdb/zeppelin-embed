"""Instrument scratch archives only; these timings are not ordinary latency."""
from pathlib import Path
import json, subprocess, hashlib, sys

r = Path(sys.argv[1])


def replace_once(s, a, b):
    assert s.count(a) == 1, (a, s.count(a))
    return s.replace(a, b)


for arm in ["before", "after"]:
    dest = r / (arm + "-diag")
    subprocess.run(["cp", "-cR", str(r / arm), str(dest)], check=True)
    p = dest / "tools/matched-api/Cargo.toml"
    s = replace_once(
        p.read_text(),
        'zeppelin-embed = { path = "../../crates/zeppelin-embed" }',
        'zeppelin-embed = { path = "../../crates/zeppelin-embed", features = ["query-timing"] }',
    )
    p.write_text(s)
    p = dest / "tools/matched-api/src/main.rs"
    s = replace_once(
        p.read_text(),
        "  let start=Instant::now();",
        '  eprintln!("ASTRA19_QUERY_BEGIN {}",q["id"]);\n  let start=Instant::now();',
    )
    s = replace_once(
        s,
        '  let hits=store.query_text(q["text"].as_str().ok_or("text")?,options)?;',
        '  let outcome=store.query_text_with_diagnostics(q["text"].as_str().ok_or("text")?,options)?;',
    )
    marker = "  let ms=start.elapsed().as_secs_f64()*1000.0;"
    addition = '''
  eprintln!("ASTRA19_QUERY_END");
  let d=outcome.diagnostics.as_ref().ok_or("diagnostics")?;
  let work=json!({"generation":d.snapshot_generation,"plan":format!("{:?}",d.plan),"approximate":d.approximate,"exact_rescore":d.exact_rescore,"counters":format!("{:?}",d.counters),"fusion":format!("{:?}",d.fusion),"materialization":format!("{:?}",d.materialization)});
  let ct=d.timings.as_ref().ok_or("core timings")?;
  let ot=outcome.timings.as_ref().ok_or("outer timings")?;
  let stages=json!({"admission_ns":ct.admission.as_nanos(),"vector_ns":ct.vector.as_nanos(),"lexical_queue_ns":ct.lexical_queue.as_nanos(),"lexical_ns":ct.lexical.as_nanos(),"fusion_ns":ct.fusion_cross_fill.as_nanos(),"embedding_queue_ns":ot.embedding_queue.as_nanos(),"embedding_evaluation_ns":ot.embedding_evaluation.as_nanos(),"tokenization_ns":ot.tokenization.as_nanos(),"normalization_ns":ot.embedding_normalization.as_nanos(),"retrieval_ns":ot.retrieval.as_nanos(),"end_to_end_ns":ot.end_to_end.as_nanos()});
  let hits=outcome.hits;
'''
    s = replace_once(s, marker, marker + addition)
    s = replace_once(
        s, '"query":q["id"],"ms":ms,',
        '"query":q["id"],"ms":ms,"work":work,"stages":stages,',
    )
    p.write_text(s)
    if arm == "after":
        p = dest / "crates/zeppelin-embed/src/lifecycle/mod.rs"
        whole = p.read_text()
        begin = whole.index("    fn search_hybrid_prepared_then<")
        end = whole.index("    /// Returns " + chr(96) + "(hits, builds)", begin)
        s = whole[begin:end]
        s = replace_once(s, "        let started = self.clock.now();",
            "        let overlap_origin = std::time::Instant::now();\n        let started = self.clock.now();")
        s = replace_once(s,
            "            let queue_time = timing_elapsed(self.clock.as_ref(), queued);",
            "            let span_start = overlap_origin.elapsed().as_nanos();\n            let queue_time = timing_elapsed(self.clock.as_ref(), queued);")
        marker = "            (\n                name,\n                result,\n                queue_time,"
        s = replace_once(s, marker,
            '            eprintln!("ASTRA19_SPAN lexical {} {}", span_start, overlap_origin.elapsed().as_nanos());\n' + marker)
        s = replace_once(s, "                    let vector_query =",
            "                    let embedding_start = overlap_origin.elapsed().as_nanos();\n                    let vector_query =")
        s = replace_once(s, "                    let mut vector_preparation = prepared::PreparedVectorQuery::new(",
            '                    eprintln!("ASTRA19_SPAN embedding {} {}", embedding_start, overlap_origin.elapsed().as_nanos());\n                    let mut vector_preparation = prepared::PreparedVectorQuery::new(')
        s = replace_once(s, "                    let result = run_vector_leg(",
            "                    let vector_start = overlap_origin.elapsed().as_nanos();\n                    let result = run_vector_leg(")
        s = replace_once(s, "                    Ok((vector_query, vector_preparation, result))",
            '                    eprintln!("ASTRA19_SPAN vector {} {}", vector_start, overlap_origin.elapsed().as_nanos());\n                    Ok((vector_query, vector_preparation, result))')
        p.write_text(whole[:begin] + s + whole[end:])

manifest = []
for cell in json.loads((r / "manifest.json").read_text()):
    if cell["api"] != "hybrid":
        continue
    cell["label"] = "diag-" + cell["label"]
    cell["command"][0] = str(r / (cell["arm"] + "-diag-api"))
    cell["command"][3] = str(r / cell["label"])
    manifest.append(cell)
(r / "diagnostic-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
(r / "diagnostic-source.json").write_text(json.dumps({
    arm: {
        str(p.relative_to(r / arm)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in (r / arm).rglob("*") if p.is_file()
    }
    for arm in ["before-diag", "after-diag"]
}, indent=2) + "\n")
print("diagnostic arms", len(manifest))

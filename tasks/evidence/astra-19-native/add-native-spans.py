"""Put native evaluation and lexical spans in one monotonic process clock."""
from pathlib import Path
import hashlib, json, sys

r = Path(sys.argv[1])
changed = {}
for arm in ["before-diag", "after-diag"]:
    root = r / arm
    p = root / "crates/zeppelin-embed/src/diag.rs"
    s = p.read_text()
    s += """
/// Scratch benchmark clock; never copied into the product source.
pub fn astra19_epoch_ns() -> u128 {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(std::time::Instant::now).elapsed().as_nanos()
}
"""
    p.write_text(s)
    changed[str(p)] = hashlib.sha256(p.read_bytes()).hexdigest()
    p = root / "crates/zeppelin-embed-text/src/ingest.rs"
    s = p.read_text()
    marker = "                        embed(role, &tokens, control.as_ref())"
    assert s.count(marker) == 1
    s = s.replace(marker, """
                        let native_start = zeppelin_embed::diag::astra19_epoch_ns();
                        let native_result = embed(role, &tokens, control.as_ref());
                        let native_end = zeppelin_embed::diag::astra19_epoch_ns();
                        eprintln!("ASTRA19_SPAN native {} {}", native_start, native_end);
                        native_result""")
    p.write_text(s)
    changed[str(p)] = hashlib.sha256(p.read_bytes()).hexdigest()
    p = root / "crates/zeppelin-embed/src/lifecycle/mod.rs"
    s = p.read_text()
    if arm == "after-diag":
        assert "let overlap_origin = std::time::Instant::now();" in s
        s = s.replace("        let overlap_origin = std::time::Instant::now();\n", "")
        s = s.replace("overlap_origin.elapsed().as_nanos()", "crate::diag::astra19_epoch_ns()")
    else:
        begin = s.index("    fn search_hybrid_then<R>")
        end = s.index("    /// Returns " + chr(96) + "(hits, builds)", begin)
        part = s[begin:end]
        marker = "            let queue_time = timing_elapsed(self.clock.as_ref(), queued);"
        assert part.count(marker) == 1
        part = part.replace(marker, "            let span_start = crate::diag::astra19_epoch_ns();\n" + marker)
        marker = "            (\n                name,\n                result,\n                queue_time,"
        assert part.count(marker) == 1
        part = part.replace(marker, '            eprintln!("ASTRA19_SPAN lexical {} {}", span_start, crate::diag::astra19_epoch_ns());\n' + marker)
        s = s[:begin] + part + s[end:]
    p.write_text(s)
    changed[str(p)] = hashlib.sha256(p.read_bytes()).hexdigest()
(r / "native-spans-source.json").write_text(json.dumps({
    "scope": "scratch diagnostic archives only; not ordinary latency binaries",
    "clock": "one std::Instant origin shared by core and embedding owner",
    "native_boundary": "actual embed method on owner; includes runtime setup/copy, not isolated device instructions",
    "files": changed,
}, indent=2) + "\n")

// Tooling-only entrypoint; not a proposed or shipped ABI. Honest C pointer/length
// inputs make the entire parser reachable in the size consumer.
#[cfg(frontend)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ze54_parse_probe(data: *const u8, len: usize) -> usize {
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    match zeppelin_embed_cypher::parse_bytes(bytes, Default::default(), &mut zeppelin_embed_cypher::Budget::default()) {
        Ok(ast) => ast.nodes().len(),
        Err(_) => 0,
    }
}

#[cfg(not(standalone))]
#[unsafe(no_mangle)]
pub extern "C" fn ze54_core_anchor() -> u32 {
    zeppelin_embed_ffi::ze_abi_version()
}

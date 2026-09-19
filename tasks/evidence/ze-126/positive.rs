use zeppelin_embed::property_graph::query::{resources::QueryMemory, ValueContext};
use zeppelin_embed_cypher::{compile_read_in, CompileLimits};
fn check(memory: &QueryMemory<'_>, context: &mut ValueContext<'_>) {
 let _ = compile_read_in("RETURN 1", &[], CompileLimits::default(), memory, context, |read, _| Ok(read.columns().len()));
}

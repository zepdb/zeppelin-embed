use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::RuntimeContext;
use zeppelin_embed::property_graph::storage::adjacency::RangeScratch;
use zeppelin_embed::property_graph::storage::tree::directory::{TreeError, TreeResources};

fn hold<'v, 'm, 'g>(
    slot: &mut Option<RuntimeContext<'v, 'm, 'g>>,
    memory: &'m QueryMemory<'g>,
) -> Result<(), TreeError> {
    let scratch = {
        let context = slot.as_mut().ok_or(TreeError::Missing)?;
        let mut resources = TreeResources::for_query(context)?;
        RangeScratch::for_query(memory, &mut resources)?
    };
    std::hint::black_box(scratch.owned_bytes());
    Ok(())
}

fn main() {}

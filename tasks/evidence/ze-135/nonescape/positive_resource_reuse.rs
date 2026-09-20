use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::RuntimeContext;
use zeppelin_embed::property_graph::storage::adjacency::RangeScratch;
use zeppelin_embed::property_graph::storage::tree::directory::{TreeError, TreeResources};

fn reuse<'a, 'v, 'm, 'g>(
    context: &'a mut RuntimeContext<'v, 'm, 'g>,
    memory: &'m QueryMemory<'g>,
) -> Result<(), TreeError>
where
    'm: 'a,
    'g: 'a,
{
    let mut resources = TreeResources::for_query(context)?;
    let scratch = RangeScratch::for_query(memory, &mut resources)?;
    resources.step(0)?;
    resources.step(0)?;
    std::hint::black_box(scratch.owned_bytes());
    Ok(())
}

fn main() {}

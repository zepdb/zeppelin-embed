use zeppelin_embed::property_graph::{NodeId, RelId};

fn main() {
    let node = NodeId::new(1).expect("nonzero node identity");
    let relationship = RelId::new(2).expect("nonzero relationship identity");
    assert_eq!(node.get(), 1);
    assert_eq!(relationship.get(), 2);
}

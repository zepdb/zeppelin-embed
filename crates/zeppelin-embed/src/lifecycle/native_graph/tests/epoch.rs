use super::recovery::{
    commit_tail_test_node, disable_generation_fixture_maintenance, file_snapshot, native_options,
    observe_node, two_epoch_fixture,
};
use crate::lifecycle::Store;

#[cfg_attr(test, test)]
pub(super) fn an_epoch_switch_on_a_graph_store_is_refused_and_the_store_reopens() {
    let (directory, a, b) = two_epoch_fixture();
    let options = native_options().with_epoch(a.clone());
    let store = Store::open(directory.path(), options.clone()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let before = file_snapshot(directory.path());
    assert!(
        matches!(
            store.switch_epoch_alias(b.identity()),
            Err(crate::epoch::EpochTransitionError::GraphEpochTransition)
        ),
        "graph catalog cannot carry epoch B"
    );
    assert!(
        matches!(
            store.drop_epoch(b.identity().embedding),
            Err(crate::epoch::EpochTransitionError::GraphEpochTransition)
        ),
        "graph store cannot drop epochs"
    );
    assert_eq!(file_snapshot(directory.path()), before);
    assert_eq!(store.epoch_identity(), Some(a.identity()));
    let node = commit_tail_test_node(&store, "after-epoch-refusal");
    let observed = observe_node(&store, node);
    assert!(observed.is_some());
    drop(store);
    let reopened = Store::open(directory.path(), options).unwrap();
    assert_eq!(observe_node(&reopened, node), observed);
}

#[cfg_attr(test, test)]
pub(super) fn an_epoch_drop_on_a_graph_store_is_refused_before_any_durable_change() {
    let (directory, a, b) = two_epoch_fixture();
    let store = Store::open(directory.path(), native_options().with_epoch(a)).unwrap();
    store.enable_graph().unwrap();
    let before = file_snapshot(directory.path());
    assert!(
        matches!(
            store.drop_epoch(b.identity().embedding),
            Err(crate::epoch::EpochTransitionError::GraphEpochTransition)
        ),
        "graph store cannot drop epochs"
    );
    assert_eq!(file_snapshot(directory.path()), before);
}

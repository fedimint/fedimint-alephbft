use crate::{
    testing::{init_log, spawn_honest_member_with_data_provider, HonestMember},
    DataProvider, NodeCount, NodeIndex, SpawnHandle,
};
use aleph_bft_mock::{Data, Router, Spawner, StalledDataProvider};
use async_trait::async_trait;
use futures::{future::pending, StreamExt};
use std::time::Duration;

/// Hands out one piece of data and then waits for more that never comes, like
/// the data provider of a node with nothing left to order.
struct OnceThenStalled(Option<Data>);

#[async_trait]
impl DataProvider<Data> for OnceThenStalled {
    async fn get_data(&mut self) -> Option<Data> {
        match self.0.take() {
            Some(data) => Some(data),
            None => pending().await,
        }
    }
}

/// Nodes only create units while the DAG holds data that has not been
/// ordered, so data a single node submits must get ordered even though no
/// other node has any data of its own.
#[tokio::test(flavor = "multi_thread")]
async fn data_of_one_node_is_ordered_while_the_others_have_none() {
    init_log();

    const DATA: Data = 42;
    let n_members = NodeCount(4);
    let spawner = Spawner::new();
    let (net_hub, networks) = Router::new(n_members, 1.0);
    spawner.spawn("network-hub", net_hub);

    let mut members = Vec::new();
    for (network, _) in networks {
        let ix = network.index();
        let member = if ix == NodeIndex(0) {
            spawn_honest_member_with_data_provider(
                spawner,
                ix,
                n_members,
                vec![],
                network,
                OnceThenStalled(Some(DATA)),
            )
        } else {
            spawn_honest_member_with_data_provider(
                spawner,
                ix,
                n_members,
                vec![],
                network,
                StalledDataProvider::new(),
            )
        };
        members.push(member);
    }

    for HonestMember {
        finalization_rx, ..
    } in &mut members
    {
        let finalized = tokio::time::timeout(Duration::from_secs(30), finalization_rx.next())
            .await
            .expect("the data of one node is ordered");
        assert_eq!(finalized, Some(DATA));
    }

    for HonestMember {
        exit_tx, handle, ..
    } in members
    {
        let _ = exit_tx.send(());
        let _ = handle.await;
    }
}

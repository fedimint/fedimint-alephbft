use crate::{
    units::{FullUnit, PreUnit, SignedUnit},
    Data, DataProvider, Hasher, MultiKeychain, NodeIndex, Receiver, Sender, SessionId, Signed,
    Terminator,
};
use futures::{future::pending, pin_mut, FutureExt, StreamExt};
use log::{debug, error};
use std::marker::PhantomData;

/// The component responsible for packing Data from DataProvider into received PreUnits,
/// and signing the outcome, thus creating SignedUnits that are sent back to Runway.
pub struct Packer<H, D, DP, MK>
where
    H: Hasher,
    D: Data,
    DP: DataProvider<D>,
    MK: MultiKeychain,
{
    data_provider: DP,
    preunits_from_runway: Receiver<PreUnit<H>>,
    signed_units_for_runway: Sender<SignedUnit<H, D, MK>>,
    pending_data_from_runway: Receiver<usize>,
    /// The latest number of units with unordered data in the DAG.
    pending_data: usize,
    keychain: MK,
    session_id: SessionId,
    _phantom: PhantomData<D>,
}

impl<H, D, DP, MK> Packer<H, D, DP, MK>
where
    H: Hasher,
    D: Data,
    DP: DataProvider<D>,
    MK: MultiKeychain,
{
    pub fn new(
        data_provider: DP,
        preunits_from_runway: Receiver<PreUnit<H>>,
        signed_units_for_runway: Sender<SignedUnit<H, D, MK>>,
        pending_data_from_runway: Receiver<usize>,
        keychain: MK,
        session_id: SessionId,
    ) -> Self {
        Self {
            data_provider,
            preunits_from_runway,
            signed_units_for_runway,
            pending_data_from_runway,
            pending_data: 0,
            keychain,
            session_id,
            _phantom: PhantomData,
        }
    }

    fn index(&self) -> NodeIndex {
        self.keychain.index()
    }

    /// The main loop.
    async fn pack(&mut self) {
        loop {
            // the order is important: first wait for a PreUnit, then ask for fresh Data
            let preunit = match self.preunits_from_runway.next().await {
                Some(preunit) => preunit,
                None => {
                    error!(target: "AlephBFT-packer", "{:?} Runway PreUnit stream closed.", self.index());
                    break;
                }
            };
            debug!(target: "AlephBFT-packer", "{:?} Received PreUnit.", self.index());
            // A data provider may wait until it has data, so that no units are
            // created while there is nothing to order. Units with data already
            // in the DAG still need our units to get ordered, so while there
            // are any we create our unit without data instead of waiting.
            let data = futures::select_biased! {
                data = self.data_provider.get_data().fuse() => data,
                () = wait_for_pending_data(
                    &mut self.pending_data_from_runway,
                    &mut self.pending_data,
                ).fuse() => None,
            };
            debug!(target: "AlephBFT-packer", "{:?} Received data.", self.index());
            let full_unit = FullUnit::new(preunit, data, self.session_id);
            let signed_unit = Signed::sign(full_unit, &self.keychain);
            if self
                .signed_units_for_runway
                .unbounded_send(signed_unit)
                .is_err()
            {
                error!(target: "AlephBFT-packer", "{:?} Could not send SignedUnit to Runway.", self.index());
                break;
            }
        }
    }

    /// Run the main loop until receiving a signal to exit.
    pub async fn run(&mut self, mut terminator: Terminator) -> Result<(), ()> {
        debug!(target: "AlephBFT-packer", "{:?} Packer started.", self.index());
        let pack = self.pack().fuse();
        pin_mut!(pack);

        futures::select! {
            _ = pack => Err(()),
            _ = terminator.get_exit().fuse() => {
                terminator.terminate_sync().await;
                Ok(())
            },
        }
    }
}

/// Returns once the DAG holds a unit with data that has not been ordered yet.
async fn wait_for_pending_data(
    pending_data_from_runway: &mut Receiver<usize>,
    pending_data: &mut usize,
) {
    loop {
        while let Ok(Some(count)) = pending_data_from_runway.try_next() {
            *pending_data = count;
        }

        if *pending_data > 0 {
            return;
        }

        match pending_data_from_runway.next().await {
            Some(count) => *pending_data = count,
            // The runway is gone, so the session is ending.
            None => pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Packer;
    use crate::{
        units::{ControlHash, PreUnit, SignedUnit},
        NodeCount, NodeIndex, Receiver, Sender, SessionId, Terminator,
    };
    use aleph_bft_mock::{Data, DataProvider, Hasher64, Keychain, StalledDataProvider};
    use aleph_bft_types::NodeMap;
    use futures::{
        channel::{mpsc, oneshot},
        pin_mut, FutureExt, StreamExt,
    };

    const SESSION_ID: SessionId = 43;
    const NODE_ID: NodeIndex = NodeIndex(0);
    const N_MEMBERS: NodeCount = NodeCount(4);

    struct Preliminaries {
        preunits_channel: Sender<PreUnit<Hasher64>>,
        signed_units_channel: Receiver<SignedUnit<Hasher64, Data, Keychain>>,
        packer: Packer<Hasher64, Data, DataProvider, Keychain>,
        terminator: (Terminator, oneshot::Sender<()>),
        preunit: PreUnit<Hasher64>,
        _pending_data_channel: Sender<usize>,
    }

    fn prepare(keychain: Keychain) -> Preliminaries {
        let data_provider = DataProvider::new();
        let (preunits_channel, preunits_from_runway) = mpsc::unbounded();
        let (signed_units_for_runway, signed_units_channel) = mpsc::unbounded();
        let (pending_data_channel, pending_data_from_runway) = mpsc::unbounded();
        let packer = Packer::new(
            data_provider,
            preunits_from_runway,
            signed_units_for_runway,
            pending_data_from_runway,
            keychain,
            SESSION_ID,
        );
        let (_exit_tx, exit_rx) = oneshot::channel();
        let parent_map = NodeMap::with_size(N_MEMBERS);
        let control_hash = ControlHash::new(&parent_map);
        let terminator = Terminator::create_root(exit_rx, "AlephBFT-packer");
        let preunit = PreUnit::new(NODE_ID, 0, control_hash);
        Preliminaries {
            preunits_channel,
            signed_units_channel,
            packer,
            terminator: (terminator, _exit_tx),
            preunit,
            _pending_data_channel: pending_data_channel,
        }
    }

    #[tokio::test]
    async fn unit_packed() {
        let keychain = Keychain::new(N_MEMBERS, NODE_ID);
        let Preliminaries {
            preunits_channel,
            signed_units_channel,
            mut packer,
            terminator,
            preunit,
            ..
        } = prepare(keychain);
        let packer_handle = packer.run(terminator.0).fuse();
        preunits_channel
            .unbounded_send(preunit.clone())
            .expect("Packer PreUnit channel closed");
        pin_mut!(packer_handle);
        pin_mut!(signed_units_channel);
        let unit = futures::select! {
            unit = signed_units_channel.next() => match unit {
                Some(unit) => unit,
                None => panic!("Packer SignedUnit channel closed"),
            },
            _ = packer_handle => panic!("Packer terminated early"),
        }
        .into_unchecked()
        .into_signable();
        assert_eq!(SESSION_ID, unit.session_id());
        assert_eq!(unit.as_pre_unit(), &preunit);
    }

    #[tokio::test]
    async fn preunits_channel_closed() {
        let keychain = Keychain::new(N_MEMBERS, NODE_ID);
        let Preliminaries {
            mut packer,
            terminator,
            ..
        } = prepare(keychain);
        assert_eq!(packer.run(terminator.0).await, Err(()));
    }

    #[tokio::test]
    async fn signed_units_channel_closed() {
        let keychain = Keychain::new(N_MEMBERS, NODE_ID);
        let Preliminaries {
            preunits_channel,
            mut packer,
            terminator,
            preunit,
            ..
        } = prepare(keychain);
        preunits_channel
            .unbounded_send(preunit)
            .expect("Packer PreUnit channel closed");
        assert_eq!(packer.run(terminator.0).await, Err(()));
    }

    #[tokio::test]
    async fn handles_requests_concurrently() {
        let keychain = Keychain::new(N_MEMBERS, NODE_ID);
        let data_provider = StalledDataProvider::new();
        let (preunits_channel, preunits_from_runway) = mpsc::unbounded::<PreUnit<Hasher64>>();
        let (signed_units_for_runway, _signed_units_channel) = mpsc::unbounded();
        let (_pending_data_channel, pending_data_from_runway) = mpsc::unbounded();
        let mut packer = Packer::new(
            data_provider,
            preunits_from_runway,
            signed_units_for_runway,
            pending_data_from_runway,
            keychain,
            SESSION_ID,
        );
        let (exit_tx, exit_rx) = oneshot::channel();
        let parent_map = NodeMap::with_size(N_MEMBERS);
        let control_hash = ControlHash::new(&parent_map);
        let preunit = PreUnit::new(NODE_ID, 0, control_hash);
        let packer_handle = packer.run(Terminator::create_root(exit_rx, "AlephBFT-packer"));
        for _ in 0..3 {
            preunits_channel
                .unbounded_send(preunit.clone())
                .expect("Packer PreUnit channel closed");
        }
        // in spite of StalledDataProvider halting Provider's pack loop,
        // we expect the component to handle the exit request immediately
        exit_tx.send(()).expect("Packer exit channel closed");
        // let's send more PreUnits just to be sure that we can
        for _ in 0..3 {
            preunits_channel
                .unbounded_send(preunit.clone())
                .expect("Packer PreUnit channel closed");
        }
        packer_handle
            .await
            .expect("Packer terminated with an error");
    }

    /// A packer whose data provider waits for data, with its channels.
    struct StalledPacker {
        packer: Packer<Hasher64, Data, StalledDataProvider, Keychain>,
        preunits_channel: Sender<PreUnit<Hasher64>>,
        signed_units_channel: Receiver<SignedUnit<Hasher64, Data, Keychain>>,
        pending_data_channel: Sender<usize>,
    }

    fn stalled_packer() -> StalledPacker {
        let (preunits_channel, preunits_from_runway) = mpsc::unbounded();
        let (signed_units_for_runway, signed_units_channel) = mpsc::unbounded();
        let (pending_data_channel, pending_data_from_runway) = mpsc::unbounded();
        let packer = Packer::new(
            StalledDataProvider::new(),
            preunits_from_runway,
            signed_units_for_runway,
            pending_data_from_runway,
            Keychain::new(N_MEMBERS, NODE_ID),
            SESSION_ID,
        );
        StalledPacker {
            packer,
            preunits_channel,
            signed_units_channel,
            pending_data_channel,
        }
    }

    fn preunit() -> PreUnit<Hasher64> {
        let parent_map = NodeMap::with_size(N_MEMBERS);
        PreUnit::new(NODE_ID, 0, ControlHash::new(&parent_map))
    }

    #[tokio::test]
    async fn waits_for_data_while_none_is_pending() {
        let StalledPacker {
            mut packer,
            preunits_channel,
            mut signed_units_channel,
            pending_data_channel,
        } = stalled_packer();
        let (_exit_tx, exit_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = packer
                .run(Terminator::create_root(exit_rx, "AlephBFT-packer"))
                .await;
        });

        preunits_channel.unbounded_send(preunit()).unwrap();
        pending_data_channel.unbounded_send(0).unwrap();

        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(100),
            signed_units_channel.next()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn packs_a_unit_without_data_while_data_is_pending() {
        let StalledPacker {
            mut packer,
            preunits_channel,
            mut signed_units_channel,
            pending_data_channel,
        } = stalled_packer();
        let (_exit_tx, exit_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = packer
                .run(Terminator::create_root(exit_rx, "AlephBFT-packer"))
                .await;
        });

        preunits_channel.unbounded_send(preunit()).unwrap();
        pending_data_channel.unbounded_send(1).unwrap();

        let unit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            signed_units_channel.next(),
        )
        .await
        .expect("a unit is packed while data is pending")
        .expect("Packer SignedUnit channel closed")
        .into_unchecked()
        .into_signable();
        assert_eq!(unit.data(), &None);
    }
}

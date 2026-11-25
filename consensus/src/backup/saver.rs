use async_trait::async_trait;

use crate::{
    units::UncheckedSignedUnit, Data, Hasher, NodeIndex, Receiver, Sender, Signature, Terminator,
};
use codec::Encode;
use futures::{AsyncWrite, AsyncWriteExt, FutureExt, StreamExt};
use log::{debug, error};

const LOG_TARGET: &str = "AlephBFT-backup-saver";

#[async_trait]
/// Write backups to persistent storage.
///
/// The blanket implementation calls [`AsyncWriteExt::flush`] after every
/// append. If that does not make writes durable for your storage backend,
/// provide a custom implementation that does.
pub trait BackupWriter {
    /// Append new data and persist it along with any earlier appends before returning success.
    async fn append(&mut self, data: &[u8]) -> std::io::Result<()>;

    /// Append data, allowing it to be buffered when `flush` is false.
    ///
    /// When `flush` is true, persist this data and all earlier appends in
    /// order before returning success. The saver sets `flush` for local units
    /// because broadcasting one and then losing it in a crash could cause the
    /// node to sign a second unit for the same round.
    ///
    /// The default calls [`Self::append`], so writers that always flush do
    /// not need to override this method.
    async fn append_with_flush(&mut self, data: &[u8], flush: bool) -> std::io::Result<()>
    where
        Self: Send,
    {
        let _ = flush;
        self.append(data).await
    }
}

#[async_trait]
impl<W: AsyncWrite + Send + Unpin> BackupWriter for W {
    async fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.write_all(data).await?;
        self.flush().await
    }
}

/// Component responsible for saving units into backup.
/// It waits for items to appear on its receivers, and writes them to backup.
/// It announces a successful write through an appropriate response sender.
pub struct BackupSaver<H: Hasher, D: Data, S: Signature, W: BackupWriter> {
    units_from_runway: Receiver<UncheckedSignedUnit<H, D, S>>,
    responses_for_runway: Sender<UncheckedSignedUnit<H, D, S>>,
    backup: W,
    index: NodeIndex,
}

impl<H: Hasher, D: Data, S: Signature, W: BackupWriter + Send> BackupSaver<H, D, S, W> {
    pub fn new(
        units_from_runway: Receiver<UncheckedSignedUnit<H, D, S>>,
        responses_for_runway: Sender<UncheckedSignedUnit<H, D, S>>,
        backup: W,
        index: NodeIndex,
    ) -> BackupSaver<H, D, S, W> {
        BackupSaver {
            units_from_runway,
            responses_for_runway,
            backup,
            index,
        }
    }

    pub async fn save_item(
        &mut self,
        item: &UncheckedSignedUnit<H, D, S>,
    ) -> Result<(), std::io::Error> {
        let flush = item.as_signable().creator() == self.index;
        self.backup.append_with_flush(&item.encode(), flush).await
    }

    pub async fn run(&mut self, mut terminator: Terminator) {
        let mut terminator_exit = false;
        loop {
            futures::select! {
                unit = self.units_from_runway.next() => {
                    let item = match unit {
                        Some(unit) => unit,
                        None => {
                            error!(target: LOG_TARGET, "receiver of units to save closed early");
                            break;
                        },
                    };
                    if let Err(e) = self.save_item(&item).await {
                        error!(target: LOG_TARGET, "couldn't save item to backup: {:?}", e);
                        break;
                    }
                    if self.responses_for_runway.unbounded_send(item).is_err() {
                        error!(target: LOG_TARGET, "couldn't respond with saved unit to runway");
                        break;
                    }
                },
                _ = terminator.get_exit().fuse() => {
                    debug!(target: LOG_TARGET, "backup saver received exit signal.");
                    terminator_exit = true;
                }
            }

            if terminator_exit {
                debug!(target: LOG_TARGET, "backup saver decided to exit.");
                terminator.terminate_sync().await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{
        channel::{mpsc, oneshot},
        StreamExt,
    };

    use aleph_bft_mock::{Data, Hasher64, Keychain, Saver, Signature};

    use super::{BackupSaver, BackupWriter};
    use crate::{
        units::{creator_set, preunit_to_unchecked_signed_unit, UncheckedSignedUnit},
        NodeCount, NodeIndex, Terminator,
    };

    /// Implements only the original writer method.
    struct LegacyWriter(Saver);

    #[async_trait::async_trait]
    impl BackupWriter for LegacyWriter {
        async fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
            self.0.append(data).await
        }
    }

    type TestBackupSaver = BackupSaver<Hasher64, Data, Signature, LegacyWriter>;
    type TestUnit = UncheckedSignedUnit<Hasher64, Data, Signature>;
    struct PrepareSaverResponse<F: futures::Future> {
        task: F,
        units_for_saver: mpsc::UnboundedSender<TestUnit>,
        units_from_saver: mpsc::UnboundedReceiver<TestUnit>,
        exit_tx: oneshot::Sender<()>,
    }

    fn prepare_saver() -> PrepareSaverResponse<impl futures::Future> {
        let (units_for_saver, units_from_runway) = mpsc::unbounded();
        let (units_for_runway, units_from_saver) = mpsc::unbounded();
        let (exit_tx, exit_rx) = oneshot::channel();
        let backup = LegacyWriter(Saver::new());

        let task = {
            let mut saver: TestBackupSaver =
                BackupSaver::new(units_from_runway, units_for_runway, backup, NodeIndex(0));

            async move {
                saver.run(Terminator::create_root(exit_rx, "saver")).await;
            }
        };

        PrepareSaverResponse {
            task,
            units_for_saver,
            units_from_saver,
            exit_tx,
        }
    }

    #[tokio::test]
    async fn test_proper_relative_responses_ordering() {
        let PrepareSaverResponse {
            task,
            units_for_saver,
            mut units_from_saver,
            exit_tx,
        } = prepare_saver();

        let handle = tokio::spawn(async {
            task.await;
        });

        let creators = creator_set(NodeCount(5));
        let keychains: Vec<_> = (0..5)
            .map(|id| Keychain::new(NodeCount(5), NodeIndex(id)))
            .collect();
        let units: Vec<TestUnit> = (0..5)
            .map(|k| {
                preunit_to_unchecked_signed_unit(
                    creators[k].create_unit(0).unwrap().0,
                    0,
                    &keychains[k],
                )
            })
            .collect();

        for u in units.iter() {
            units_for_saver.unbounded_send(u.clone()).unwrap();
        }

        for u in units {
            let u_backup = units_from_saver.next().await.unwrap();
            assert_eq!(u, u_backup);
        }

        exit_tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Records the `flush` flag of every append.
    #[derive(Clone, Default)]
    struct FlushRecorder(std::sync::Arc<std::sync::Mutex<Vec<bool>>>);

    #[async_trait::async_trait]
    impl BackupWriter for FlushRecorder {
        async fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
            self.append_with_flush(data, true).await
        }

        async fn append_with_flush(&mut self, _data: &[u8], flush: bool) -> std::io::Result<()> {
            self.0.lock().unwrap().push(flush);
            Ok(())
        }
    }

    #[tokio::test]
    async fn async_write_keeps_flushing_each_append() {
        let mut writer = futures::io::BufWriter::new(Vec::new());
        writer.append(b"legacy").await.unwrap();
        assert_eq!(writer.get_ref(), b"legacy");
        writer.append_with_flush(b" remote", false).await.unwrap();
        assert_eq!(writer.get_ref(), b"legacy remote");
        writer.append_with_flush(b" local", true).await.unwrap();
        assert_eq!(writer.get_ref(), b"legacy remote local");
    }

    #[tokio::test]
    async fn only_own_units_are_flushed() {
        let n_members = NodeCount(5);
        let own_index = NodeIndex(2);
        let (units_for_saver, units_from_runway) = mpsc::unbounded();
        let (units_for_runway, mut units_from_saver) = mpsc::unbounded();
        let (exit_tx, exit_rx) = oneshot::channel();
        let recorder = FlushRecorder::default();
        let mut saver: BackupSaver<Hasher64, Data, Signature, FlushRecorder> = BackupSaver::new(
            units_from_runway,
            units_for_runway,
            recorder.clone(),
            own_index,
        );
        let handle = tokio::spawn(async move {
            saver.run(Terminator::create_root(exit_rx, "saver")).await;
        });

        let creators = creator_set(n_members);
        for (k, creator) in creators.iter().enumerate() {
            let keychain = Keychain::new(n_members, NodeIndex(k));
            let unit =
                preunit_to_unchecked_signed_unit(creator.create_unit(0).unwrap().0, 0, &keychain);
            units_for_saver.unbounded_send(unit).unwrap();
        }
        for _ in 0..n_members.0 {
            units_from_saver.next().await.unwrap();
        }

        let expected: Vec<bool> = (0..n_members.0)
            .map(|k| NodeIndex(k) == own_index)
            .collect();
        assert_eq!(*recorder.0.lock().unwrap(), expected);

        exit_tx.send(()).unwrap();
        handle.await.unwrap();
    }
}

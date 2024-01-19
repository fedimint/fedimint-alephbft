use std::io::Write;

use crate::{units::UncheckedSignedUnit, Data, Hasher, Receiver, Sender};
use aleph_bft_types::{MultiKeychain, Terminator};
use async_trait::async_trait;
use codec::Encode;
use futures::{FutureExt, StreamExt};
use log::{debug, error};

use crate::{alerts::AlertData, backup::BackupItem};

const LOG_TARGET: &str = "AlephBFT-backup-saver";

#[async_trait]
/// Write backups to peristent storage.
pub trait BackupWriter {
    /// Append new data to the backup.
    async fn append(&mut self, data: &[u8]) -> std::io::Result<()>;
}

#[async_trait]
impl<W: Write + Send> BackupWriter for W {
    async fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        Write::write_all(self, data)?;
        self.flush()
    }
}

/// Component responsible for saving units and alert data into backup.
/// It waits for items to appear on its receivers, and writes them to backup.
/// It announces a successful write through an appropriate response sender.
pub struct BackupSaver<H: Hasher, D: Data, MK: MultiKeychain, W: BackupWriter> {
    units_from_runway: Receiver<UncheckedSignedUnit<H, D, MK::Signature>>,
    data_from_alerter: Receiver<AlertData<H, D, MK>>,
    responses_for_runway: Sender<UncheckedSignedUnit<H, D, MK::Signature>>,
    responses_for_alerter: Sender<AlertData<H, D, MK>>,
    backup: W,
}

impl<H: Hasher, D: Data, MK: MultiKeychain, W: BackupWriter> BackupSaver<H, D, MK, W> {
    pub fn new(
        units_from_runway: Receiver<UncheckedSignedUnit<H, D, MK::Signature>>,
        data_from_alerter: Receiver<AlertData<H, D, MK>>,
        responses_for_runway: Sender<UncheckedSignedUnit<H, D, MK::Signature>>,
        responses_for_alerter: Sender<AlertData<H, D, MK>>,
        backup: W,
    ) -> BackupSaver<H, D, MK, W> {
        BackupSaver {
            units_from_runway,
            data_from_alerter,
            responses_for_runway,
            responses_for_alerter,
            backup,
        }
    }

    pub async fn save_item(&mut self, item: BackupItem<H, D, MK>) -> Result<(), std::io::Error> {
        self.backup.append(&item.encode()).await
    }

    pub async fn run(&mut self, mut terminator: Terminator) {
        let mut terminator_exit = false;
        loop {
            futures::select! {
                unit = self.units_from_runway.next() => {
                    let unit = match unit {
                        Some(unit) => unit,
                        None => {
                            error!(target: LOG_TARGET, "receiver of units to save closed early");
                            break;
                        },
                    };
                    let item = BackupItem::Unit(unit.clone());
                    if let Err(e) = self.save_item(item).await {
                        error!(target: LOG_TARGET, "couldn't save item to backup: {:?}", e);
                        break;
                    }
                    if self.responses_for_runway.unbounded_send(unit).is_err() {
                        error!(target: LOG_TARGET, "couldn't respond with saved unit to runway");
                        break;
                    }
                },
                data = self.data_from_alerter.next() => {
                    let data = match data {
                        Some(data) => data,
                        None => {
                            error!(target: LOG_TARGET, "receiver of alert data to save closed early");
                            break;
                        },
                    };
                    let item = BackupItem::AlertData(data.clone());
                    if let Err(e) = self.save_item(item).await {
                        error!(target: LOG_TARGET, "couldn't save item to backup: {:?}", e);
                        break;
                    }
                    if self.responses_for_alerter.unbounded_send(data).is_err() {
                        error!(target: LOG_TARGET, "couldn't respond with saved alert data to runway");
                        break;
                    }
                }
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
    use aleph_bft_types::Terminator;

    use crate::{
        alerts::{Alert, AlertData},
        backup::BackupSaver,
        units::{creator_set, preunit_to_unchecked_signed_unit, UncheckedSignedUnit},
        NodeCount, NodeIndex,
    };

    type TestBackupSaver = BackupSaver<Hasher64, Data, Keychain, Saver>;
    type TestUnit = UncheckedSignedUnit<Hasher64, Data, Signature>;
    type TestAlertData = AlertData<Hasher64, Data, Keychain>;
    struct PrepareSaverResponse<F: futures::Future> {
        task: F,
        units_for_saver: mpsc::UnboundedSender<TestUnit>,
        units_from_saver: mpsc::UnboundedReceiver<TestUnit>,
        alerts_for_saver: mpsc::UnboundedSender<TestAlertData>,
        alerts_from_saver: mpsc::UnboundedReceiver<TestAlertData>,
        exit_tx: oneshot::Sender<()>,
    }

    enum Item {
        Alert,
        Unit,
    }

    fn prepare_saver() -> PrepareSaverResponse<impl futures::Future> {
        let (units_for_saver, units_from_runway) = mpsc::unbounded();
        let (units_for_runway, units_from_saver) = mpsc::unbounded();
        let (alerts_for_saver, alerts_from_alerter) = mpsc::unbounded();
        let (alerts_for_alerter, alerts_from_saver) = mpsc::unbounded();
        let (exit_tx, exit_rx) = oneshot::channel();
        let backup = Saver::new();

        let task = {
            let mut saver: TestBackupSaver = BackupSaver::new(
                units_from_runway,
                alerts_from_alerter,
                units_for_runway,
                alerts_for_alerter,
                backup,
            );

            async move {
                saver.run(Terminator::create_root(exit_rx, "saver")).await;
            }
        };

        PrepareSaverResponse {
            task,
            units_for_saver,
            units_from_saver,
            alerts_for_saver,
            alerts_from_saver,
            exit_tx,
        }
    }

    #[tokio::test]
    async fn test_proper_relative_responses_ordering() {
        let PrepareSaverResponse {
            task,
            units_for_saver,
            mut units_from_saver,
            alerts_for_saver,
            mut alerts_from_saver,
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
        let alerts: Vec<TestAlertData> = (0..5)
            .map(|k| {
                TestAlertData::OwnAlert(Alert::new(
                    NodeIndex(0),
                    (units[k].clone(), units[k].clone()),
                    vec![],
                ))
            })
            .collect();

        let backup_save_ordering = vec![
            Item::Unit,
            Item::Alert,
            Item::Alert,
            Item::Alert,
            Item::Unit,
            Item::Unit,
            Item::Alert,
            Item::Unit,
            Item::Alert,
            Item::Unit,
        ];
        let mut units_iter = units.iter();
        let mut alerts_iter = alerts.iter();

        for i in backup_save_ordering {
            match i {
                Item::Unit => units_for_saver
                    .unbounded_send(units_iter.next().unwrap().clone())
                    .unwrap(),
                Item::Alert => alerts_for_saver
                    .unbounded_send(alerts_iter.next().unwrap().clone())
                    .unwrap(),
            }
        }

        for u in units {
            let u_backup = units_from_saver.next().await.unwrap();
            assert_eq!(u, u_backup);
        }

        for a in alerts {
            let a_backup = alerts_from_saver.next().await.unwrap();
            assert_eq!(a, a_backup);
        }

        exit_tx.send(()).unwrap();
        handle.await.unwrap();
    }
}

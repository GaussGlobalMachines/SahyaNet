use crate::{
    ApplicationConfig, Supervisor,
    engine_client::EngineClient,
    finalizer::Finalizer,
    ingress::{Mailbox, Message},
};
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::{Result, anyhow};
use commonware_macros::select;
use commonware_runtime::{Clock, Handle, Metrics, Spawner, Storage};
use commonware_utils::SystemTimeExt;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::{self, Either, try_join},
};
use rand::Rng;
use seismicbft_syncer::{Orchestrator, ingress::Mailbox as SyncerMailbox};

use futures::task::{Context, Poll};
use seismicbft_types::{Block, Digest};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{error, info, warn};

// Define a future that checks if the oneshot channel is closed using a mutable reference
struct ChannelClosedFuture<'a, T> {
    sender: &'a mut oneshot::Sender<T>,
}

impl<T> futures::Future for ChannelClosedFuture<'_, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Use poll_canceled to check if the receiver has dropped the channel
        match self.sender.poll_canceled(cx) {
            Poll::Ready(()) => Poll::Ready(()), // Receiver dropped, channel closed
            Poll::Pending => Poll::Pending,     // Channel still open
        }
    }
}

// Helper function to create the future using a mutable reference
fn oneshot_closed_future<T>(sender: &mut oneshot::Sender<T>) -> ChannelClosedFuture<T> {
    ChannelClosedFuture { sender }
}

pub struct Actor<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock + Rng> {
    context: R,
    mailbox: mpsc::Receiver<Message>,
    engine_client: EngineClient,
    forkchoice: Arc<Mutex<ForkchoiceState>>,
    built_block: Arc<Mutex<Option<Block>>>,
    finalizer: Option<Finalizer<R>>,
    tx_height_notify: mpsc::Sender<(u64, oneshot::Sender<()>)>,
}

impl<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock + Rng> Actor<R> {
    pub async fn new(context: R, cfg: ApplicationConfig) -> (Self, Mailbox, Supervisor) {
        let (tx, rx) = mpsc::channel(cfg.mailbox_size);

        let genesis_hash = Block::genesis_hash();
        let forkchoice = Arc::new(Mutex::new(ForkchoiceState {
            head_block_hash: genesis_hash.into(),
            safe_block_hash: genesis_hash.into(),
            finalized_block_hash: genesis_hash.into(),
        }));

        let engine_client = EngineClient::new(cfg.engine_url.clone(), &cfg.engine_jwt);
        let (finalizer, tx_height_notify) = Finalizer::new(
            context.with_label("finalizer"),
            engine_client.clone(),
            forkchoice.clone(),
            cfg.partition_prefix,
        )
        .await;

        (
            Self {
                context,
                mailbox: rx,
                engine_client, // todo: why are these types dif?
                forkchoice,
                built_block: Arc::new(Mutex::new(None)),
                finalizer: Some(finalizer),
                tx_height_notify,
            },
            Mailbox::new(tx),
            Supervisor::new(cfg.participants),
        )
    }

    pub fn start(
        mut self,
        syncer: seismicbft_syncer::Mailbox,
        orchestrator: Orchestrator,
        finalizer_network: (mpsc::Sender<()>, mpsc::Receiver<()>),
    ) -> Handle<()> {
        self.context.spawn_ref()(self.run(syncer, orchestrator, finalizer_network))
    }

    pub async fn run(
        mut self,
        mut syncer: SyncerMailbox,
        orchestrator: Orchestrator,
        (_, rx_finalizer): (mpsc::Sender<()>, mpsc::Receiver<()>),
    ) {
        self.finalizer
            .take()
            .expect("no finalizer")
            .start(orchestrator, rx_finalizer);

        let rand_id: u8 = rand::random();
        while let Some(message) = self.mailbox.next().await {
            match message {
                Message::Genesis { response } => {
                    info!("Handling message Genesis");
                    let _ = response.send(Block::genesis_hash().into());
                }
                Message::Propose {
                    view,
                    parent,
                    mut response,
                } => {
                    info!("{rand_id} Handling message Propose view: {}", view);

                    let built = self.built_block.clone();
                    select! {
                              res = self.handle_proposal(parent, &mut syncer) => {

                                  match res {
                                      Ok(block) => {
                                          // store block
                                          let digest = block.digest;
                                          {
                                              let mut built = built.lock().expect("locked poisoned");
                                              *built = Some(block);
                                          }

                                          // send digest to consensus
                                          let _ = response.send(digest);
                                      },
                                      Err(e) => warn!("Failed to create a block for height {view}: {e}")
                                  }
                              },
                              _ = oneshot_closed_future(&mut response) => {
                                  // simplex dropped reciever
                                  warn!(view, "proposal aborted");
                              }
                    //      }
                      }
                    //  });
                }
                Message::Broadcast { payload } => {
                    info!("{rand_id} Handling message Broadcast");
                    let Some(built_block) =
                        self.built_block.lock().expect("poisoned mutex").clone()
                    else {
                        warn!("Asked to broadcast a block with no built block");
                        continue;
                    };
                    // todo(dalton): This should be a hard assert but for testing im just going to log
                    if payload != built_block.digest {
                        error!(
                            "The payload we were asked to broadcast is different then our built block"
                        );
                    }

                    syncer.broadcast(built_block).await;
                }

                Message::Verify {
                    view,
                    parent,
                    payload,
                    mut response,
                } => {
                    info!("{rand_id} Handling message Verify view: {}", view);
                    // Get the parent block
                    let parent_request = if parent.1 == Block::genesis_hash().into() {
                        Either::Left(future::ready(Ok(Block::genesis())))
                    } else {
                        Either::Right(syncer.get(Some(parent.0), parent.1).await)
                    };

                    let block_request = syncer.get(None, payload).await;

                    // Wait for the blocks to be available or the request to be cancelled in a separate task (to
                    // continue processing other messages)
                    self.context.with_label("verify").spawn({
                        let mut syncer = syncer.clone();
                        move |_| async move {
                            let requester = try_join(parent_request, block_request);
                            select! {
                                result = requester => {
                                    let (parent, block) = result.unwrap();
                                    if handle_verify(&block, parent) {

                                        // persist valid block
                                        syncer.store_verified(view, block).await;

                                        // respond
                                        let _ = response.send(true);
                                    } else {
                                        info!("Unsucceful vote");
                                        let _ = response.send(false);
                                    }
                                },
                                _ = oneshot_closed_future(&mut response) => {
                                    warn!(view, "verify aborted");
                                }
                            }
                        }
                    });
                }
            }
        }
    }

    async fn handle_proposal(
        &mut self,
        parent: (u64, Digest),
        syncer: &mut seismicbft_syncer::Mailbox,
    ) -> Result<Block> {
        // Get the parent block
        let parent_request = if parent.1 == Block::genesis_hash().into() {
            Either::Left(future::ready(Ok(Block::genesis())))
        } else {
            Either::Right(syncer.get(Some(parent.0), parent.1).await)
        };

        let parent = parent_request.await.unwrap();

        // now that we have the parent additionally await for that to be executed by the finalizer
        let (tx, rx) = oneshot::channel();
        self.tx_height_notify
            .try_send((parent.height, tx))
            .expect("finalizer dropped");

        // await for notification
        let _ = rx.await.expect("Finalizer dropped");

        let mut current = self.context.current().epoch_millis();
        if current <= parent.timestamp {
            current = parent.timestamp + 1;
        }
        let forkchoice_clone;
        {
            forkchoice_clone = *self.forkchoice.lock().expect("poisoned");
        }

        let payload_id = self
            .engine_client
            .start_building_block(forkchoice_clone, current)
            .await
            .ok_or(anyhow!("Unable to build payload"))?;

        self.context.sleep(Duration::from_millis(50)).await;

        let payload_envelope = self.engine_client.get_payload(payload_id).await;

        let block = Block::compute_digest(
            parent.digest,
            parent.height + 1,
            current,
            payload_envelope.envelope_inner.execution_payload,
            payload_envelope.execution_requests.to_vec(),
            payload_envelope.envelope_inner.block_value,
        );

        Ok(block)
    }
}

fn handle_verify(block: &Block, parent: Block) -> bool {
    if block.eth_parent_hash() != parent.eth_block_hash() {
        return false;
    }
    if block.parent != parent.digest {
        tracing::error!("2");
        return false;
    }
    if block.height != parent.height + 1 {
        tracing::error!("3");
        return false;
    }
    if block.timestamp <= parent.timestamp {
        tracing::error!("4");
        return false;
    }
    true
}

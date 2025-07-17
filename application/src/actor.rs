use crate::{
    ApplicationConfig, Supervisor,
    engine_client::EngineClient,
    ingress::{Mailbox, Message},
};
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::{Result, anyhow};
use commonware_macros::select;
use commonware_runtime::{Clock, Handle, Metrics, Spawner, Storage};
use commonware_storage::metadata::{self, Metadata};
use commonware_utils::{SystemTimeExt, array::FixedBytes, hex};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::{self, Either, try_join},
};
use rand::Rng;
use seismicbft_syncer::{Orchestrator, ingress::Mailbox as SyncerMailbox};

use futures::task::{Context, Poll};
use seismicbft_types::Block;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{debug, error, info, warn};

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
    // Finalizer storage
    finalizer_metadata: Option<Metadata<R, FixedBytes<1>>>,
}

impl<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock + Rng> Actor<R> {
    pub async fn new(context: R, cfg: ApplicationConfig) -> (Self, Mailbox, Supervisor) {
        let (tx, rx) = mpsc::channel(cfg.mailbox_size);

        let genesis_hash = Block::genesis_hash();
        let forkchoice = ForkchoiceState {
            head_block_hash: genesis_hash.into(),
            safe_block_hash: genesis_hash.into(),
            finalized_block_hash: genesis_hash.into(),
        };

        // Initialize finalizer metadata
        let finalizer_metadata = Metadata::init(
            context.with_label("finalizer_metadata"),
            metadata::Config {
                partition: format!("{}-finalizer_metadata", cfg.partition_prefix),
            },
        )
        .await
        .expect("Failed to initialize finalizer metadata");

        (
            Self {
                context,
                mailbox: rx,
                engine_client: EngineClient::new(cfg.engine_url, &cfg.engine_jwt), // todo: why are these types dif?
                forkchoice: Arc::new(Mutex::new(forkchoice)),
                built_block: Arc::new(Mutex::new(None)),
                finalizer_metadata: Some(finalizer_metadata),
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
        mut orchestrator: Orchestrator,
        (tx_finalizer, mut rx_finalizer): (mpsc::Sender<()>, mpsc::Receiver<()>),
    ) {
        let mut finalizer_metadata = self.finalizer_metadata.take().unwrap();
        let engine_client = self.engine_client.clone();
        let forkchoice_state = self.forkchoice.clone();
        self.context
            .with_label("finalizer")
            .spawn(move |_| async move {
                // Initialize last indexed from metadata store
                let latest_key = FixedBytes::new([0u8]);
                let mut last_indexed = if let Some(bytes) = finalizer_metadata.get(&latest_key) {
                    u64::from_be_bytes(bytes.to_vec().try_into().unwrap())
                } else {
                    0
                };

                // Index all finalized blocks.
                //
                // If using state sync, this is not necessary.
                loop {
                    // Check if the next block is available
                    let next = last_indexed + 1;
                    if let Some(block) = orchestrator.get(next).await {
                        // check the payload
                        let payload_status = engine_client.check_payload(&block).await;

                        if payload_status.is_valid() {
                            // its valid so commit the block
                            let eth_hash = block.eth_block_hash();
                            info!("Commiting block 0x{} for height {}", hex(&eth_hash), next);

                            let forkchoice = ForkchoiceState {
                                head_block_hash: eth_hash.into(),
                                safe_block_hash: eth_hash.into(),
                                finalized_block_hash: eth_hash.into(),
                            };

                            engine_client.commit_hash(forkchoice).await;

                            *forkchoice_state.lock().expect("poisoned") = forkchoice;

                            finalizer_metadata.put(latest_key.clone(), next.to_be_bytes().to_vec());
                            finalizer_metadata
                                .sync()
                                .await
                                .expect("Failed to sync finalizer");

                            // Update the latest indexed
                            //self.contiguous_height.set(next as i64);
                            last_indexed = next;
                            info!(height = next, "indexed finalized block");

                            orchestrator.processed(next, block.digest).await;
                            continue;
                        }

                        // In an application that maintains state, you would compute the state transition function here.
                        //
                        // After an unclean shutdown (where the finalizer metadata is not synced after some height is processed by the application),
                        // it is possible that the application may be asked to process a block it has already seen (which it can simply ignore).

                        // Update finalizer metadata.
                        //
                        // If we updated the finalizer metadata before the application applied its state transition function, an unclean
                        // shutdown could put the application in an unrecoverable state where the last indexed height (the height we
                        // start processing at after restart) is ahead of the application's last processed height (requiring the application
                        // to process a non-contiguous log). For the same reason, the application should sync any cached disk changes after processing
                        // its state transition function to ensure that the application can continue processing from the the last synced indexed height
                        // (on restart).

                        // Update last view processed (if we have a finalization for this block)
                    }

                    // Try to connect to our latest handled block (may not exist finalizations for some heights)
                    if orchestrator.repair(next).await {
                        continue;
                    }

                    // If nothing to do, wait for some message from someone that the finalized store was updated
                    debug!(height = next, "waiting to index finalized block");
                    let _ = rx_finalizer.next().await;
                }
            });

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

                    // Get the parent block
                    let parent_request = if parent.1 == Block::genesis_hash().into() {
                        Either::Left(future::ready(Ok(Block::genesis())))
                    } else {
                        Either::Right(syncer.get(Some(parent.0), parent.1).await)
                    };

                    //  let forkchoice = self.forkchoice.lock().unwrap().clone();
                    // Wait for the parent block to be available or the request to be cancelled in a separate task (to
                    // continue processing other messages)
                    self.context.with_label("propose").spawn({
                        let built = self.built_block.clone();
                        let engine_client = self.engine_client.clone();
                        let forkchoice = self.forkchoice.clone();
                        move |context| async move {
                            select! {
                                parent = parent_request => {
                                    let parent = parent.unwrap();

                                    match handle_proposal(context, view, forkchoice.clone(), parent, engine_client).await {
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
                            }
                        }
                    });
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
                    // let ack = buffer.broadcast(Recipients::All, built_block).await;

                    // drop(ack);
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
}

async fn handle_proposal<R: Rng + Spawner + Metrics + Clock>(
    context: R,
    view: u64,
    forkchoice: Arc<Mutex<ForkchoiceState>>,
    parent: Block,
    engine_client: EngineClient,
) -> Result<Block> {
    // let now = SystemTime::now();
    // let mut timestamp = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    // do timestamp
    let mut current = context.current().epoch_millis();
    if current <= parent.timestamp {
        current = parent.timestamp + 1;
    }
    let forkchoice_clone;
    {
        forkchoice_clone = *forkchoice.lock().expect("poisoned");
    }
    let payload_id = engine_client
        .start_building_block(forkchoice_clone, current)
        .await
        .ok_or(anyhow!("Unable to build payload"))?;

    context.sleep(Duration::from_millis(50)).await;

    let payload_envelope = engine_client.get_payload(payload_id).await;

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

fn handle_verify(block: &Block, parent: Block) -> bool {
    if block.eth_parent_hash() != parent.eth_block_hash() {
        return false;
    }
    if block.parent != parent.digest {
        return false;
    }
    if block.height != parent.height + 1 {
        return false;
    }
    if block.timestamp <= parent.timestamp {
        return false;
    }
    true
}

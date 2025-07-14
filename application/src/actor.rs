use crate::{
    ApplicationConfig, Supervisor,
    engine_client::EngineClient,
    ingress::{Mailbox, Message},
};
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::{Context as _, Result, anyhow};
use commonware_broadcast::{Broadcaster as _, buffered};
use commonware_consensus::simplex::types::View;
use commonware_cryptography::sha256::Digest;
use commonware_p2p::Recipients;
use commonware_runtime::{Clock, Handle, Metrics, Spawner};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::try_join,
};
use rand::Rng;
use seismicbft_syncer::ingress::Mailbox as SyncerMailbox;

use futures::task::{Context, Poll};
use seismicbft_types::{Block, PublicKey};
use std::{
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{error, warn};

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

pub struct Actor<R: Rng + Spawner + Metrics + Clock> {
    context: R,
    mailbox: mpsc::Receiver<Message>,
    engine_client: EngineClient,
    forkchoice: ForkchoiceState,
    built_block: Option<Block>,
}

impl<R: Rng + Spawner + Metrics + Clock> Actor<R> {
    pub fn new(context: R, cfg: ApplicationConfig) -> (Self, Mailbox, Supervisor) {
        let (tx, rx) = mpsc::channel(cfg.mailbox_size);
        (
            Self {
                context,
                mailbox: rx,
                engine_client: EngineClient::new(cfg.engine_url, &cfg.engine_jwt), // todo: why are these types dif?
                forkchoice: ForkchoiceState::default(),
                built_block: None,
            },
            Mailbox::new(tx),
            Supervisor::new(cfg.participants),
        )
    }

    pub fn start(
        mut self,
        syncer: seismicbft_syncer::Mailbox,
        buffer: buffered::Mailbox<PublicKey, Block>,
    ) -> Handle<()> {
        self.context.spawn_ref()(self.run(syncer, buffer))
    }

    pub async fn run(
        mut self,
        mut syncer: SyncerMailbox,
        mut buffer: buffered::Mailbox<PublicKey, Block>,
    ) {
        while let Some(message) = self.mailbox.next().await {
            match message {
                Message::Genesis { response } => {
                    // todo
                    let _ = response.send([0; 32].into());
                }
                Message::Propose {
                    view,
                    parent,
                    mut response,
                } => {
                    tokio::select! {
                        res = self.handle_proposal(view, parent, &mut syncer) => {
                            match res {
                                Ok(block) => {
                                    let block_digest = block.digest;

                                    // store block
                                    self.built_block = Some(block);

                                    // respond to simplex with the digest. Simplex will then ask syncer to broadcast
                                    let _ = response.send(block_digest);
                                }
                                Err(e) => warn!("Failed to generate a proposal for view {view} : {e}")
                            }

                        }
                        _ = oneshot_closed_future(&mut response) => {
                            // Simplex dropped the receiver
                            warn!(view, "proposal aborted");
                        }
                    }
                }
                Message::Broadcast { payload } => {
                    let Some(built_block) = self.built_block.clone() else {
                        warn!("Asked to broadcast a block with no built block");
                        continue;
                    };

                    // todo(dalton): This should be a hard assert but for testing im just going to log
                    if payload != built_block.digest {
                        error!(
                            "The payload we were asked to broadcast is different then our built block"
                        );
                    }

                    let ack = buffer.broadcast(Recipients::All, built_block).await;
                    drop(ack);
                }

                Message::Verify {
                    view,
                    parent,
                    payload,
                    mut response,
                } => {
                    tokio::select!(
                        res = self.handle_verify(view, parent, payload, &mut syncer) => {
                            match res {
                                Ok(valid) => {
                                    let _ =  response.send(valid);
                                }
                                Err(e) => {
                                    // todo(dalton): Should we vote no here? As it stands now the only codepath here is if we fail to get both the parent and the current block from the syncer
                                    warn!(view, "failed to handle verify: {e}")
                                }
                            }
                        }
                        _ = oneshot_closed_future(&mut response) => {
                            warn!(view, "verify aborted")
                        }

                    )
                }
                Message::Finalize {
                    view,
                    parent,
                    payload,
                } => {
                    if let Err(e) = self
                        .handle_finalize(view, parent, payload, &mut syncer)
                        .await
                    {
                        error!(
                            "Critical error, failed to finalize view {view} with payload: {payload} \n {e}"
                        );
                        // todo: We should probably panic here this will most likely be unrecoverable
                    }
                }
            }
        }
    }

    async fn handle_proposal(
        &mut self,
        view: u64,
        parent: (u64, Digest),
        syncer: &mut SyncerMailbox,
    ) -> Result<Block> {
        // Get parent block
        let parent_block = syncer.get(Some(parent.0), parent.1).await.await?;

        let now = SystemTime::now();

        let mut timestamp = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        if timestamp <= parent_block.timestamp {
            timestamp = parent_block.timestamp + 1;
        }

        self.forkchoice.head_block_hash = parent_block.eth_block_hash().into();

        let payload_id = self
            .engine_client
            .start_building_block(self.forkchoice, timestamp)
            .await
            .ok_or(anyhow!("Unable to build payload"))?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload_envelope = self.engine_client.get_payload(payload_id).await;

        let block = Block::compute_digest(
            parent.1,
            view,
            timestamp,
            payload_envelope.envelope_inner.execution_payload,
            payload_envelope.execution_requests.to_vec(),
            payload_envelope.envelope_inner.block_value,
        );
        Ok(block)
    }

    async fn handle_verify(
        &mut self,
        view: u64,
        parent: (u64, Digest),
        payload: Digest,
        syncer: &mut SyncerMailbox,
    ) -> Result<bool> {
        let parent_block_fut = syncer.get(Some(parent.0), parent.1).await;
        let block_fut = syncer.get(None, payload).await;

        let (parent, block) = try_join(parent_block_fut, block_fut)
            .await
            .context("Unable to get block and parent from syncer to verify")?;

        if block.eth_parent_hash() != parent.eth_block_hash() {
            return Ok(false);
        }
        if block.parent != parent.digest {
            return Ok(false);
        }
        if block.height != parent.height + 1 {
            return Ok(false);
        }
        if block.timestamp <= parent.timestamp {
            return Ok(false);
        }
        // todo(dalton): We should have a check that this new block isnt from too far into the future. With some allowed varience

        let payload_status = self.engine_client.check_payload(&block).await;

        if payload_status.is_valid() {
            // send to syncer to be stored
            syncer.store_verified(view, block).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn handle_finalize(
        &mut self,
        view: View,
        _parent: View,
        payload: Digest,
        syncer: &mut SyncerMailbox,
    ) -> Result<()> {
        let block = syncer.get(Some(view), payload).await.await?;

        let eth_hash = block.eth_block_hash();
        self.forkchoice.safe_block_hash = eth_hash.into();
        self.forkchoice.finalized_block_hash = eth_hash.into();

        self.engine_client.commit_hash(self.forkchoice).await;

        Ok(())
    }
}

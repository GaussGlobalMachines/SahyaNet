use std::{collections::BTreeSet, time::Duration};

use crate::{
    Orchestration, Orchestrator,
    coordinator::Coordinator,
    handler::Handler,
    ingress::{Mailbox, Message},
    key::{MultiIndex, Value},
};
use commonware_broadcast::{Broadcaster as _, buffered};
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_consensus::simplex::types::Viewable as _;
use commonware_macros::select;
use commonware_p2p::{Receiver, Recipients, Sender, utils::requester};
use commonware_resolver::{Resolver as _, p2p};
use commonware_runtime::{Clock, Handle, Metrics, Spawner, Storage};
use commonware_storage::{
    archive::{self, Archive, Identifier},
    index::translator::{EightCap, TwoCap},
};
use futures::{StreamExt as _, channel::mpsc};
use governor::Quota;
use rand::Rng;
use seismicbft_types::{
    Block, Digest, FinalizationArchive, Finalized, NAMESPACE, Notarized, PublicKey,
};
use tracing::{debug, warn};

const REPLAY_BUFFER: usize = 8 * 1024 * 1024;
const REPLAY_CONCURRENCY: usize = 4;
const WRITE_BUFFER: usize = 1024 * 1024;

pub struct Actor<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock> {
    context: R,
    mailbox: mpsc::Receiver<Message>,
    orchestrator_mailbox: mpsc::Receiver<Orchestration>,
    // Blocks verified stored by view<>digest
    verified: Archive<TwoCap, R, Digest, Block>,
    // Blocks notarized stored by view<>digest
    notarized: Archive<TwoCap, R, Digest, Notarized>,

    // Finalizations stored by height
    finalized: Archive<EightCap, R, Digest, FinalizationArchive>,
    // Blocks finalized stored by height
    //
    // We store this separately because we may not have the finalization for a block
    blocks: Archive<EightCap, R, Digest, Block>,
    public_key: PublicKey,
    participants: Vec<PublicKey>,
    mailbox_size: usize,
    backfill_quota: Quota,
    activity_timeout: u64,
}

impl<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock + Rng> Actor<R> {
    pub async fn new(context: R, config: crate::Config) -> (Self, Mailbox, Orchestrator) {
        let (tx, rx) = mpsc::channel(config.mailbox_size);

        // todo: mess with these defaults
        let verified_archive = Archive::init(
            context.with_label("verified_archive"),
            archive::Config {
                translator: TwoCap,
                partition: format!("{}-verified-archive", config.partition_prefix),
                compression: None,
                codec_config: (),
                section_mask: 0xffff_ffff_ffff_f000u64,
                pending_writes: 0,
                write_buffer: WRITE_BUFFER,
                replay_concurrency: REPLAY_CONCURRENCY,
                replay_buffer: REPLAY_BUFFER,
            },
        )
        .await
        .expect("failed to init verified archive");

        // Initialize notarized blocks
        let notarized_archive = Archive::init(
            context.with_label("notarized_archive"),
            archive::Config {
                partition: format!("{}-notarizations", config.partition_prefix),
                translator: TwoCap,
                section_mask: 0xffff_ffff_ffff_f000u64,
                pending_writes: 0,
                replay_concurrency: REPLAY_CONCURRENCY,
                compression: None,
                codec_config: (),
                replay_buffer: REPLAY_BUFFER,
                write_buffer: WRITE_BUFFER,
            },
        )
        .await
        .expect("Failed to initialize notarized archive");

        // Initialize finalizations
        let finalized_archive = Archive::init(
            context.with_label("finalized_archive"),
            archive::Config {
                partition: format!("{}-finalizations", config.partition_prefix),
                translator: EightCap,
                section_mask: 0xffff_ffff_fff0_0000u64,
                pending_writes: 0,
                replay_concurrency: REPLAY_CONCURRENCY,
                compression: None,
                codec_config: (),
                replay_buffer: REPLAY_BUFFER,
                write_buffer: WRITE_BUFFER,
            },
        )
        .await
        .expect("Failed to initialize finalized archive");

        // Initialize blocks
        let block_archive = Archive::init(
            context.with_label("block_archive"),
            archive::Config {
                partition: format!("{}-blocks", config.partition_prefix),
                translator: EightCap,
                section_mask: 0xffff_ffff_fff0_0000u64,
                pending_writes: 0,
                replay_concurrency: REPLAY_CONCURRENCY,
                compression: None,
                codec_config: (),
                replay_buffer: REPLAY_BUFFER,
                write_buffer: WRITE_BUFFER,
            },
        )
        .await
        .expect("Failed to initialize finalized archive");

        let (orchestrator_sender, orchestrator_receiver) = mpsc::channel(2); // buffer to send processed while moving forward
        let orchestrator = Orchestrator::new(orchestrator_sender);

        (
            Self {
                context,
                mailbox: rx,
                orchestrator_mailbox: orchestrator_receiver,
                verified: verified_archive,
                notarized: notarized_archive,
                finalized: finalized_archive,
                blocks: block_archive,
                public_key: config.public_key,
                participants: config.participants,
                mailbox_size: config.mailbox_size,
                backfill_quota: config.backfill_quota,
                activity_timeout: config.activity_timeout,
            },
            Mailbox::new(tx),
            orchestrator,
        )
    }

    pub fn start(
        mut self,
        buffer: buffered::Mailbox<PublicKey, Block>,
        backfill: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        tx_finalizer: mpsc::Sender<()>,
    ) -> Handle<()> {
        self.context.spawn_ref()(self.run(buffer, backfill, tx_finalizer))
    }

    pub async fn run(
        mut self,
        mut buffer: buffered::Mailbox<PublicKey, Block>,
        backfill: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        mut tx_finalizer: mpsc::Sender<()>,
    ) {
        let coordinator = Coordinator::new(self.participants.clone());
        let (handler_sender, mut handler_receiver) = mpsc::channel(self.mailbox_size);
        let handler = Handler::new(handler_sender);

        let (resolver_engine, mut resolver) = p2p::Engine::new(
            self.context.with_label("resolver"),
            p2p::Config {
                coordinator,
                consumer: handler.clone(),
                producer: handler,
                mailbox_size: self.mailbox_size,
                requester_config: requester::Config {
                    public_key: self.public_key.clone(),
                    rate_limit: self.backfill_quota,
                    initial: Duration::from_secs(1),
                    timeout: Duration::from_secs(2),
                },
                fetch_retry_timeout: Duration::from_millis(100), // prevent busy loop
                priority_requests: false,
                priority_responses: false,
            },
        );

        resolver_engine.start(backfill);

        let mut latest_view = 0;
        let mut requested_blocks = BTreeSet::new();
        let mut last_view_processed: u64 = 0;
        let mut outstanding_notarize = BTreeSet::new();
        loop {
            // Cancel useless requests
            let mut to_cancel = Vec::new();
            outstanding_notarize.retain(|view| {
                if *view < latest_view {
                    to_cancel.push(MultiIndex::new(Value::Notarized(*view)));
                    false
                } else {
                    true
                }
            });
            for view in to_cancel {
                resolver.cancel(view).await;
            }

            select! {
                mailbox_message = self.mailbox.next() => {
                    let message = mailbox_message.expect("Mailbox closed");
                    match message {
                    Message::Get {
                        view,
                        payload,
                        response,
                    } => {
                        // Check if in buffer
                        if let Some(buffered) = buffer
                            .get(None, payload, Some(payload))
                            .await
                            .into_iter()
                            .next()
                        {
                            debug!(height = buffered.height, "found block in buffer");
                            let _ = response.send(buffered);
                            continue;
                        }

                        // check verified blocks
                        if let Some(block) = self
                            .verified
                            .get(Identifier::Key(&payload))
                            .await
                            .expect("Failed to read verified block store")
                        {
                            debug!(height = block.height, "found block in verified");
                            let _ = response.send(block);
                            continue;
                        }

                        // check notarized blocks
                        if let Some(notarization) = self.notarized.get(Identifier::Key(&payload)).await.expect("Failed to get notarized block"){
                            let block = notarization.block;
                            debug!(height = block.height, "found block in notarized");
                            let _ = response.send(block);
                            continue;

                        }

                        // check finalized blocks
                        if let Some(block) = self.blocks.get(Identifier::Key(&payload)).await.expect("Failed to get finalized block") {
                            debug!(height = block.height, "found block in finalized");
                            let _ = response.send(block);
                            continue;
                        }

                        // Fetch from network if notarized (view is non-nil)
                        if let Some(view) = view {
                            debug!(view, ?payload, "required block missing");
                            resolver.fetch(MultiIndex::new(Value::Notarized(view))).await;
                        }

                        buffer
                            .subscribe_prepared(None, payload, Some(payload), response)
                            .await;
                    }
                    Message::Broadcast { payload } => {
                        let ack = buffer.broadcast(Recipients::All, payload).await;

                        drop(ack);
                    }
                    Message::StoreVerified { view, payload } => {
                        match self.verified.put(view, payload.digest, payload).await {
                            Ok(_) => {
                                debug!(view, "verified block stored");
                            }
                            Err(archive::Error::AlreadyPrunedTo(_)) => {
                                debug!(view, "verified block already pruned");
                            }
                            Err(e) => {
                                panic!("Failed to insert verified block: {e}");
                            }
                        }
                    }
                    Message::Finalize {finalization} => {
                        let view = finalization.view();
                        // Check if in buffer
                        let proposal = &finalization.proposal;
                        let mut block = buffer.get(None, proposal.payload, Some(proposal.payload)).await.into_iter().next();

                        // Check if in verified
                        if block.is_none() {
                            block = self.verified.get(Identifier::Key(&proposal.payload)).await.expect("Failed to get verified block");
                        }

                        // Check if in notarized
                        if block.is_none() {
                            block = self.notarized.get(Identifier::Key(&proposal.payload)).await.expect("Failed to get notarized block").map(|notarized| notarized.block);
                        }

                        if let Some(block) = block {
                            let digest = proposal.payload;
                            let height = block.height;

                            // persist the finalization
                            self.finalized.put(height, digest, FinalizationArchive { finalization }).await.expect("Failed to insert into finalization store");
                            self.blocks.put(height, digest,block).await.expect("failed to insert into block store");

                            // prune blocks
                            let min_view = last_view_processed.saturating_sub(self.activity_timeout);
                            self.verified.prune(min_view).await.expect("Failed to prune verified blocks");
                            self.notarized.prune(min_view).await.expect("Failed to prune notarized blocks");

                            // notify finalizer
                            let _ = tx_finalizer.try_send(());

                            // update latest
                            latest_view = view;

                            continue;
                        }

                        // Fetch from network
                        warn!(view, digest = ?proposal.payload, "finalized block missing");
                        resolver.fetch(MultiIndex::new(Value::Digest(proposal.payload))).await;
                    }
                    Message::Notarize{notarization} => {
                        let view = notarization.view();
                        // Check if in buffer
                        let proposal = &notarization.proposal;
                        let mut block =  buffer.get(None, proposal.payload, Some(proposal.payload)).await.into_iter().next();

                        // Check if in verified blocks
                        if block.is_none() {
                            block = self.verified.get(Identifier::Key(&proposal.payload)).await.expect("Failed to get verified block");
                        }

                        if let Some(block) = block {
                            let height = block.height;
                            let digest = proposal.payload;
                            let notarization = Notarized::new(notarization, block);

                            // Persist the notarization
                            match self.notarized.put(view,digest,notarization).await {
                                Ok(_) => {
                                    debug!(view,height, "notarized block stored")
                                }
                                Err(archive::Error::AlreadyPrunedTo(_)) => {
                                    debug!(view, "notarized already pruned");
                                }
                                Err(e) => {
                                    panic!("Failed to insert notarized block: {e}");
                                }
                            };
                            continue;
                        }

                        debug!(view, "notarized block missing");
                        outstanding_notarize.insert(view);
                        resolver.fetch(MultiIndex::new(Value::Notarized(view))).await;

                    }
                }
                },
                orchestrator_message = self.orchestrator_mailbox.next() => {
                    let orchestrator_message = orchestrator_message.expect("Orchestrator closed");
                    match orchestrator_message {
                        crate::Orchestration::Get { next, result } => {
                            // Check if in blocks
                            let block = self.blocks.get(Identifier::Index(next)).await.expect("Failed to get finalized block");
                            result.send(block).expect("Failed to send block");
                        },
                        crate::Orchestration::Processed { next, digest } => {
                            // Cancel any outstanding requests (by height and by digest)
                            resolver.cancel(MultiIndex::new(Value::Finalized(next))).await;
                            resolver.cancel(MultiIndex::new(Value::Digest(digest))).await;

                            // If finalization exists, mark as last_view_processed
                            let finalization = self.finalized.get(Identifier::Index(next)).await.expect("Failed to get finalized block");
                            if let Some(finalization) = finalization {
                                last_view_processed = finalization.finalization.view();
                            }

                            // Drain requested blocks less than next
                            requested_blocks.retain(|height| *height > next);
                        },
                        crate::Orchestration::Repair { next, result } => {
                            // Find next gap
                            let (_, start_next) = self.blocks.next_gap(next);
                            let Some(start_next) = start_next else {
                                result.send(false).expect("Failed to send repair result");
                                continue;
                            };

                            // If we are at some height greater than genesis, attempt to repair the parent
                            if next > 0 {
                                // Get gapped block
                                let gapped_block = self.blocks.get(Identifier::Index(start_next)).await.expect("Failed to get finalized block").expect("Gapped block missing");

                                // Attempt to repair one block from other sources
                                let target_block = gapped_block.parent;
                                let verified = self.verified.get(Identifier::Key(&target_block)).await.expect("Failed to get verified block");
                                if let Some(verified) = verified {
                                    let height = verified.height;
                                    self.blocks.put(height, target_block, verified).await.expect("Failed to insert finalized block");
                                    debug!(height, "repaired block from verified");
                                    result.send(true).expect("Failed to send repair result");
                                    continue;
                                }
                                let notarization = self.notarized.get(Identifier::Key(&target_block)).await.expect("Failed to get notarized block");
                                if let Some(notarization) = notarization {
                                    let height = notarization.block.height;
                                    self.blocks.put(height, target_block, notarization.block).await.expect("Failed to insert finalized block");
                                    debug!(height, "repaired block from notarizations");
                                    result.send(true).expect("Failed to send repair result");
                                    continue;
                                }

                                // Request the parent block digest
                                resolver.fetch(MultiIndex::new(Value::Digest(target_block))).await;
                            }

                            // Enqueue next items (by index)
                            let range = next..std::cmp::min(start_next, next + 20);
                            debug!(range.start, range.end, "requesting missing finalized blocks");
                            for height in range {
                                // Check if we've already requested
                                if requested_blocks.contains(&height) {
                                    continue;
                                }

                                // Request the block
                                let key = MultiIndex::new(Value::Finalized(height));
                                resolver.fetch(key).await;
                                requested_blocks.insert(height);
                            }
                            result.send(false).expect("Failed to send repair result");
                        },
                    }
                },
                // Handle resolver messages last
                handler_message = handler_receiver.next() => {
                    let message = handler_message.expect("Handler closed");
                    match message {
                        crate::handler::Message::Deliver { key, value, response } => {
                            match key.to_value() {
                                Value::Notarized(view) => {
                                    // Parse notarization
                                    let Ok(notarization) = Notarized::decode(value.as_ref()) else {
                                        let _ = response.send(false);
                                        continue;
                                    };

                                    if !notarization.proof.verify(NAMESPACE, &self.participants) {
                                        let _ = response.send(false);
                                        continue;
                                    }

                                    // Ensure the received payload is for the correct view
                                    if notarization.proof.view() != view {
                                        let _ = response.send(false);
                                        continue;
                                    }

                                    // Persist the notarization
                                    let _ = response.send(true);
                                    match self.notarized
                                        .put(view, notarization.block.digest, notarization)
                                        .await {
                                        Ok(_) => {
                                            debug!(view, "notarized stored");
                                        },
                                        Err(archive::Error::AlreadyPrunedTo(_)) => {
                                            debug!(view, "notarized already pruned");

                                        }
                                        Err(e) => {
                                            panic!("Failed to insert notarized block: {e}");
                                        }
                                    };
                                }
                                Value::Finalized(height) => {
                                    // Parse finalization
                                    let Ok(finalization) = Finalized::decode(value.as_ref()) else {
                                        let _ = response.send(false);
                                        continue;
                                    };
                                    if !finalization.proof.verify(NAMESPACE, &self.participants) {
                                        let _ = response.send(false);
                                        continue;
                                    }

                                    // Ensure the received payload is for the correct height
                                    if finalization.block.height != height {
                                        let _ = response.send(false);
                                        continue;
                                    }

                                    // Indicate the finalization was valid
                                    debug!(height, "received finalization");
                                    let _ = response.send(true);

                                    // Persist the finalization
                                    self.finalized
                                        .put(height, finalization.block.digest, FinalizationArchive { finalization: finalization.proof })
                                        .await
                                        .expect("Failed to insert finalization");

                                    // Persist the block
                                    self.blocks
                                        .put(height, finalization.block.digest, finalization.block)
                                        .await
                                        .expect("Failed to insert finalized block");

                                    let _ = tx_finalizer.try_send(());
                                },
                                Value::Digest(digest) => {
                                    // Parse block
                                    let Ok(block) = Block::decode(value.as_ref()) else {
                                        let _ = response.send(false);
                                        continue;
                                    };

                                    // Ensure the received payload is for the correct digest
                                    if block.digest != digest {
                                        let _ = response.send(false);
                                        continue;
                                    }

                                    // Persist the block
                                    debug!(?digest, height = block.height, "received block");
                                    let _ = response.send(true);
                                    self.blocks
                                        .put(block.height, digest, block)
                                        .await
                                        .expect("Failed to insert finalized block");

                                    let _ = tx_finalizer.try_send(());
                                },
                            }
                        }
                        crate::handler::Message::Produce { key, response } => {
                            match key.to_value() {
                                Value::Notarized(view) => {
                                    if let Some(notarized) = self.notarized.get(Identifier::Index(view)).await.expect("Failed to get notarized block") {
                                        let _ = response.send(notarized.encode().into());
                                    } else {
                                        debug!("{view} notarization missing on request");
                                    }
                                }
                                Value::Finalized(height) => {
                                    // get finalization
                                    let Some(finalization) = self.finalized.get(Identifier::Index(height)).await.expect("Failed to get finalization")else {
                                        debug!(height, "Finalization missing on request");
                                        continue;
                                    };

                                    // get block
                                    let Some(block)= self.blocks.get(Identifier::Index(height)).await.expect("Failed to get finalized block") else {
                                        debug!(height, "finalized block missing on request");
                                        continue;
                                    };

                                    // send finalization
                                    let payload = Finalized::new(finalization.finalization, block);
                                    let _ = response.send(payload.encode().into());
                                }
                                Value::Digest(digest) => {
                                    // try buffer
                                    if let Some(block) = buffer.get(None, digest, Some(digest)).await.into_iter().next() {
                                        let _ = response.send(block.encode().into());
                                        continue;
                                    }

                                    // try verified blocks
                                    if let Some(block) = self.verified.get(Identifier::Key(&digest)).await.expect("Failed to get verified block") {
                                        let _ = response.send(block.encode().into());
                                        continue;
                                    }

                                    // try notarized blocks
                                    if let Some(block) = self.notarized.get(Identifier::Key(&digest)).await.expect("Failed to get notarized block") {
                                        let _ = response.send(block.encode().into());
                                        continue;
                                    }

                                    // try blocks
                                    if let Some(block) = self.blocks.get(Identifier::Key(&digest)).await.expect("Failed to get finalized block") {
                                        let _ = response.send(block.encode().into());
                                        continue;
                                    }

                                    // No record of block
                                    debug!(?digest, "block missing on request");
                                },
                            }
                        },
                    }
                }
            }
        }
    }
}

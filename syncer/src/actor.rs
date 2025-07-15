use std::time::Duration;

use crate::{
    coordinator::Coordinator,
    handler::Handler,
    ingress::{Mailbox, Message},
    key::{MultiIndex, Value},
};
use commonware_broadcast::{Broadcaster as _, buffered};
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
use seismicbft_types::{Block, Digest, Finalized, Notarized, PublicKey};
use tracing::debug;

const REPLAY_BUFFER: usize = 8 * 1024 * 1024;
const REPLAY_CONCURRENCY: usize = 4;
const WRITE_BUFFER: usize = 1024 * 1024;

pub struct Actor<R: Storage + Metrics + Clock + Spawner + governor::clock::Clock> {
    context: R,
    mailbox: mpsc::Receiver<Message>,
    // Blocks verified stored by view<>digest
    verified: Archive<TwoCap, R, Digest, Block>,
    // Blocks notarized stored by view<>digest
    notarized: Archive<TwoCap, R, Digest, Notarized>,

    // Finalizations stored by height
    finalized: Archive<EightCap, R, Digest, Finalized>,
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
    pub async fn new(context: R, config: crate::Config) -> (Self, Mailbox) {
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

        (
            Self {
                context,
                mailbox: rx,
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
        )
    }

    pub fn start(
        mut self,
        buffer: buffered::Mailbox<PublicKey, Block>,
        backfill: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
    ) -> Handle<()> {
        self.context.spawn_ref()(self.run(buffer, backfill))
    }

    pub async fn run(
        mut self,
        mut buffer: buffered::Mailbox<PublicKey, Block>,
        backfill: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
    ) {
        let coordinator = Coordinator::new(self.participants);
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
        loop {
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
                }
                },
                // Handle resolver messages last
                handler_message = handler_receiver.next() => {
                    let message = handler_message.expect("Handler closed");
                    match message {
                        crate::handler::Message::Deliver { key, value, response } => todo!(),
                        crate::handler::Message::Produce { key, response } => todo!(),
                    }

                }
            }
        }
    }
}

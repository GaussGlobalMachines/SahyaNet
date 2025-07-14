use crate::ingress::{Mailbox, Message};
use commonware_runtime::{Clock, Handle, Metrics, Spawner, Storage};
use commonware_storage::{
    archive::{self, Archive, Identifier},
    index::translator::TwoCap,
};
use futures::{StreamExt as _, channel::mpsc};
use seismicbft_types::{Block, Digest};
use tracing::debug;

pub struct Actor<R: Storage + Metrics + Clock + Spawner> {
    context: R,
    mailbox: mpsc::Receiver<Message>,
    // Blocks verified stored by view<>digest
    verified: Archive<TwoCap, R, Digest, Block>,
}

impl<R: Storage + Metrics + Clock + Spawner> Actor<R> {
    pub async fn new(context: R) -> (Self, Mailbox) {
        let (tx, rx) = mpsc::channel(10000); // TODO: Get channel side from config

        // todo: mess with these defaults
        let cfg = archive::Config {
            translator: TwoCap,
            partition: "seismic-bft_verified-archive".into(),
            compression: None,
            codec_config: (),
            section_mask: 0xffff_ffff_ffff_f000u64,
            pending_writes: 0,
            write_buffer: 1024 * 1024,
            replay_concurrency: 4,
            replay_buffer: 8 * 1024 * 1024,
        };

        let archive = Archive::init(context.with_label("verified_archive"), cfg)
            .await
            .unwrap();

        (
            Self {
                context,
                mailbox: rx,
                verified: archive,
            },
            Mailbox::new(tx),
        )
    }

    pub fn start(mut self) -> Handle<()> {
        self.context.spawn_ref()(self.run())
    }

    pub async fn run(mut self) {
        while let Some(message) = self.mailbox.next().await {
            match message {
                Message::Get {
                    view: _,
                    payload,
                    response,
                } => {
                    // todo Check in buffer from engine

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

                    // if it wasnt in our store get it from the resolver
                    todo!()
                }
                Message::Broadcast { payload } => todo!(),
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
        }
    }
}

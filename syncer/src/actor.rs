use crate::ingress::{Mailbox, Message};
use commonware_runtime::{Metrics, Storage};
use commonware_storage::{
    archive::{self, Archive},
    index::translator::TwoCap,
};
use futures::{StreamExt as _, channel::mpsc};
use seismicbft_types::Digest;

pub struct Actor<R: Storage + Metrics> {
    mailbox: mpsc::Receiver<Message>,
    // Blocks verified stored by view<>digest
    verified: Archive<TwoCap, R, Digest, usize>,
}

impl<R: Storage + Metrics> Actor<R> {
    pub fn new() -> (Self, Mailbox) {
        //  let (tx, rx) = mpsc::channel(10000); // TODO: Get channel side from config

        todo!()
        //(Self { mailbox: rx }, Mailbox::new(tx))
    }

    pub async fn run(mut self) {
        while let Some(message) = self.mailbox.next().await {
            match message {
                Message::Get {
                    view,
                    payload,
                    response,
                } => todo!(),
                Message::Broadcast { payload } => todo!(),
                Message::StoreVerified { view, payload } => todo!(),
            }
        }
    }
}

#[test]
fn test() {
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

    //  let archive = Archive::init()
}

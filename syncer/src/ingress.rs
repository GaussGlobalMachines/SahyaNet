use commonware_cryptography::sha256::Digest;
use futures::{
    SinkExt as _,
    channel::{mpsc, oneshot},
};
use seismicbft_types::Block;

pub enum Message {
    Get {
        view: Option<u64>,
        payload: Digest,
        response: oneshot::Sender<Block>,
    },

    Broadcast {
        payload: Block,
    },

    StoreVerified {
        view: u64,
        payload: Block,
    },
}

#[derive(Clone)]
pub struct Mailbox {
    sender: mpsc::Sender<Message>,
}

impl Mailbox {
    pub fn new(sender: mpsc::Sender<Message>) -> Self {
        Self { sender }
    }

    pub async fn get(&mut self, view: Option<u64>, payload: Digest) -> oneshot::Receiver<Block> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Get {
                view,
                payload,
                response,
            })
            .await
            .expect("Failed to send get");
        receiver
    }

    pub async fn broadcast(&mut self, payload: Block) {
        self.sender
            .send(Message::Broadcast { payload })
            .await
            .expect("Failed to send broadcast");
    }

    pub async fn store_verified(&mut self, view: u64, payload: Block) {
        self.sender
            .send(Message::StoreVerified { view, payload })
            .await
            .expect("Failed to send lock");
    }
}

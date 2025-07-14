use crate::Activity;
use commonware_consensus::{
    Automaton, Relay, Reporter,
    simplex::types::{Context, View},
};
use commonware_cryptography::sha256::Digest;
use futures::{
    SinkExt,
    channel::{mpsc, oneshot},
};
use tracing::debug;

pub enum Message {
    Genesis {
        response: oneshot::Sender<Digest>,
    },
    Propose {
        view: View,
        parent: (View, Digest),
        response: oneshot::Sender<Digest>,
    },
    Broadcast {
        payload: Digest,
    },
    Verify {
        view: View,
        parent: (View, Digest),
        payload: Digest,
        response: oneshot::Sender<bool>,
    },
    Finalize {
        view: View,
        parent: View,
        payload: Digest,
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
}

impl Automaton for Mailbox {
    type Digest = Digest;
    type Context = Context<Self::Digest>;

    async fn genesis(&mut self) -> Self::Digest {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Genesis { response })
            .await
            .expect("Failed to send genesis");
        receiver.await.expect("Failed to receive genesis")
    }

    async fn propose(&mut self, context: Context<Self::Digest>) -> oneshot::Receiver<Self::Digest> {
        // If we linked payloads to their parent, we would include
        // the parent in the `Context` in the payload.
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Propose {
                view: context.view,
                parent: context.parent,
                response,
            })
            .await
            .expect("Failed to send propose");
        receiver
    }

    async fn verify(
        &mut self,
        context: Context<Self::Digest>,
        payload: Self::Digest,
    ) -> oneshot::Receiver<bool> {
        // If we linked payloads to their parent, we would verify
        // the parent included in the payload matches the provided `Context`.
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Verify {
                view: context.view,
                parent: context.parent,
                payload,
                response,
            })
            .await
            .expect("Failed to send verify");
        receiver
    }
}

impl Reporter for Mailbox {
    type Activity = Activity;

    async fn report(&mut self, activity: Self::Activity) {
        // leaving all possible activity branches just for now. I think the only one we need to care about is finalization
        // we possibly might need to care about notarization but I will need to look into simplex a bit more to know for sure
        match activity {
            Activity::Notarize(notarize) => {
                // When a single node notarizes a proposal
                debug!("Notarize Activity for view {}", notarize.proposal.view);
            }
            Activity::Notarization(notarization) => {
                // when a quorum of nodes nortarized a proposal
                debug!(
                    "Notarization Activity for view {}",
                    notarization.proposal.view
                );
            }
            Activity::Nullify(nullify) => {
                // single node votes to skip a view
                debug!("Nullify Activity for view {}", nullify.view);
            }
            Activity::Nullification(nullification) => {
                // a quorum of nodes vote to skip a view
                debug!("Nullification Activity for view {}", nullification.view);
            }
            Activity::Finalize(finalize) => {
                // a single validator finalizes a proposal
                debug!("Finalize Activity for view {}", finalize.proposal.view);
            }
            Activity::Finalization(finalization) => {
                // a quorum of validators finalize a proposal
                debug!(
                    "Finalization Activity for view {}",
                    finalization.proposal.view
                );

                // Simplex checks the signature before here so we shouldnt have to
                let _ = self
                    .sender
                    .send(Message::Finalize {
                        view: finalization.proposal.view,
                        parent: finalization.proposal.parent,
                        payload: finalization.proposal.payload,
                    })
                    .await;
            }
            Activity::ConflictingNotarize(conflicting_notarize) => {
                // Evidence byzantine behavior
                debug!(
                    "Conflicting Notarize Activity for view {}",
                    conflicting_notarize.view
                );
            }
            Activity::ConflictingFinalize(conflicting_finalize) => {
                // evidence of byzantine behavior
                debug!(
                    "Conflicting Finalize Activity for view {}",
                    conflicting_finalize.view
                );
            }
            Activity::NullifyFinalize(nullify_finalize) => {
                // evidence of byzantine behavior
                debug!(
                    "Nullify Finalize Activity for view {}",
                    nullify_finalize.proposal.view
                );
            }
        }
    }
}

impl Relay for Mailbox {
    type Digest = Digest;

    async fn broadcast(&mut self, digest: Self::Digest) {
        self.sender
            .send(Message::Broadcast { payload: digest })
            .await
            .expect("Failed to send broadcast");
    }
}

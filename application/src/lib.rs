use commonware_consensus::simplex::types::Activity as CActivity;

pub mod actor;
pub use actor::*;

pub mod ingress;
pub use ingress::Mailbox;

pub mod supervisor;
pub use supervisor::*;

pub mod config;
pub use config::*;

mod engine_client;

pub type Digest = commonware_cryptography::sha256::Digest;
pub type Activity = CActivity<commonware_cryptography::bls12381::Signature, Digest>;

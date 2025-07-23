mod block;
pub mod genesis;

pub use block::*;
pub use genesis::*;

use commonware_consensus::simplex::types::Activity as CActivity;

pub type Digest = commonware_cryptography::sha256::Digest;
pub type Activity = CActivity<commonware_cryptography::bls12381::Signature, Digest>;

pub type PublicKey = commonware_cryptography::bls12381::PublicKey;
pub type PrivateKey = commonware_cryptography::bls12381::PrivateKey;
pub type Signature = commonware_cryptography::bls12381::Signature;

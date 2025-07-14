mod block;
pub use block::Block;

use commonware_consensus::simplex::types::Activity as CActivity;

pub type Digest = commonware_cryptography::sha256::Digest;
pub type Activity = CActivity<commonware_cryptography::bls12381::Signature, Digest>;

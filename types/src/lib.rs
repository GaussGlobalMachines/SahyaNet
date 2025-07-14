mod block;
use std::{net::SocketAddr, path::PathBuf};

pub use block::Block;

use commonware_codec::DecodeExt;
use commonware_consensus::simplex::types::Activity as CActivity;
use commonware_utils::from_hex_formatted;

pub type Digest = commonware_cryptography::sha256::Digest;
pub type Activity = CActivity<commonware_cryptography::bls12381::Signature, Digest>;

pub type PublicKey = commonware_cryptography::bls12381::PublicKey;
pub type PrivateKey = commonware_cryptography::bls12381::PrivateKey;

pub const NAMESPACE: &[u8] = b"_SEISMIC_BFT";

// temporary until we have true genesis file
pub struct GenesisCommittee {
    pub validators: Vec<(PublicKey, SocketAddr)>,
}

impl GenesisCommittee {
    pub fn load_from_file(path: PathBuf) -> Self {
        let file_string = std::fs::read_to_string(path).unwrap();

        let parsed: toml::Value = toml::from_str(&file_string).unwrap();

        let nodes: Vec<Vec<String>> = parsed["validators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arr| {
                arr.as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect()
            })
            .collect();

        let validators: Vec<(PublicKey, SocketAddr)> = nodes
            .into_iter()
            .map(|mut v| {
                let public_key_bytes = from_hex_formatted(&v.remove(0)).unwrap();
                let pub_key = PublicKey::decode(&*public_key_bytes).unwrap();

                let ip: SocketAddr = v.remove(0).parse().unwrap();

                (pub_key, ip)
            })
            .collect();

        GenesisCommittee { validators }
    }
}

use std::{num::NonZeroU32, time::Duration};

use anyhow::{Context, Result};
use commonware_codec::{Decode as _, DecodeExt as _};
use commonware_cryptography::bls12381::primitives::{
    group::{self, Share},
    poly::{self, Poly},
    variant::MinPk,
};
use commonware_utils::{from_hex_formatted, quorum};
use governor::Quota;
use summit_types::{Genesis, Identity, PrivateKey, PublicKey};

use crate::{keys::read_ed_key_from_path, utils::get_expanded_path};

/* DEFAULTS */
pub const PENDING_CHANNEL: u32 = 0;
pub const RECOVERED_CHANNEL: u32 = 1;
pub const RESOLVER_CHANNEL: u32 = 2;
pub const BROADCASTER_CHANNEL: u32 = 3;
pub const BACKFILLER_CHANNEL: u32 = 4;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_CONCURRENT: usize = 4;
const MAX_FETCH_COUNT: usize = 16;
const MAX_FETCH_SIZE: usize = 512 * 1024;
const MAILBOX_SIZE: usize = 16384;
const DEQUE_SIZE: usize = 10;
pub const MESSAGE_BACKLOG: usize = 16384;
const BACKFILL_QUOTA: u32 = 10; // in seconds
const FETCH_RATE_P2P: u32 = 128; // in seconds

pub struct EngineConfig {
    pub partition_prefix: String,
    pub signer: PrivateKey,
    pub participants: Vec<PublicKey>,
    pub mailbox_size: usize,
    pub backfill_quota: Quota,
    pub deque_size: usize,

    pub leader_timeout: Duration,
    pub notarization_timeout: Duration,
    pub nullify_retry: Duration,
    pub fetch_timeout: Duration,
    pub activity_timeout: u64,
    pub skip_timeout: u64,
    pub max_fetch_count: usize,
    pub _max_fetch_size: usize,
    pub fetch_concurrent: usize,
    pub fetch_rate_per_peer: Quota,

    pub engine_url: String,
    pub engine_jwt: String,
    pub namespace: String,
    pub genesis_hash: [u8; 32],
    pub share: Share,
    pub polynomial: Poly<Identity>,
}

impl EngineConfig {
    pub fn get_engine_config(
        engine_url: String,
        engine_jwt_path: String,
        key_path: String,
        poly_share_path: String,
        participants: Vec<PublicKey>,
        db_prefix: String,
        genesis: &Genesis,
    ) -> Result<Self> {
        // todo(dalton): clean this mess up
        // read JWT from file
        let jwt_path = get_expanded_path(&engine_jwt_path).context("failed to expand jwt path")?;
        let engine_jwt = std::fs::read_to_string(jwt_path).context("failed to load jwt")?;
        let share_path = get_expanded_path(&poly_share_path).context("failed to expand share path")?;
        let share_hex = std::fs::read_to_string(share_path).context("failed to load share hex")?;

        let share = from_hex_formatted(&share_hex).expect("invalid format for polynomial share");
        let share = group::Share::decode(share.as_ref()).expect("Could not parse share");

        // read private key from file
        let signer = read_ed_key_from_path(&key_path).context("failed to load signer key")?;
        let polynomial = from_hex_formatted(&genesis.identity).expect("Could not parse polynomial");
        let threshold = quorum(participants.len() as u32);
        let polynomial =
            poly::Public::<MinPk>::decode_cfg(polynomial.as_ref(), &(threshold as usize))
                .expect("polynomial is invalid");
        Ok(Self {
            partition_prefix: db_prefix,
            signer,
            participants,
            mailbox_size: MAILBOX_SIZE,
            backfill_quota: Quota::per_second(NonZeroU32::new(BACKFILL_QUOTA).unwrap()),
            deque_size: DEQUE_SIZE,
            leader_timeout: Duration::from_millis(genesis.leader_timeout_ms),
            notarization_timeout: Duration::from_millis(genesis.notarization_timeout_ms),
            nullify_retry: Duration::from_millis(genesis.nullify_timeout_ms),
            fetch_timeout: FETCH_TIMEOUT,
            activity_timeout: genesis.activity_timeout_views,
            skip_timeout: genesis.skip_timeout_views,
            max_fetch_count: MAX_FETCH_COUNT,
            _max_fetch_size: MAX_FETCH_SIZE,
            fetch_concurrent: FETCH_CONCURRENT,
            fetch_rate_per_peer: Quota::per_second(NonZeroU32::new(FETCH_RATE_P2P).unwrap()),
            engine_url,
            engine_jwt,
            namespace: genesis.namespace.clone(),
            genesis_hash: from_hex_formatted(&genesis.eth_genesis_hash)
                .map(|hash_bytes| hash_bytes.try_into())
                .expect("bad eth_genesis_hash")
                .expect("bad eth_genesis_hash"),
            polynomial,
            share,
        })
    }
}

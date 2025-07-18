use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr as _,
};

use clap::{Args, Parser, Subcommand};
use commonware_cryptography::Signer;
use commonware_p2p::authenticated;
use commonware_runtime::{Handle, Metrics as _, Runner, Spawner as _, tokio};
use commonware_utils::union_unique;
use futures::future::try_join_all;
use governor::Quota;
use seismicbft_types::{GenesisCommittee, NAMESPACE, PublicKey};
use tracing::{Level, error};

use crate::{
    config::{
        BACKFILLER_CHANNEL, BROADCASTER_CHANNEL, EngineConfig, MAX_MESSAGE_SIZE, MESSAGE_BACKLOG,
        RESOLVER_CHANNEL, VOTER_CHANNEL,
    },
    engine::Engine,
    keys::KeySubCmd,
};

//use crate::keys::KeySubCmd;

pub const DEFAULT_KEY_PATH: &str = "~/.seismic/consensus/key.pem";
pub const DEFAULT_DB_FOLDER: &str = "~/.seismic/consensus/store";

#[derive(Parser, Debug)]
pub struct CliArgs {
    #[command(subcommand)]
    pub cmd: Command,

    #[command(flatten)]
    pub flags: Flags,
}

impl CliArgs {
    pub fn exec(&self) {
        self.cmd.exec(&self.flags)
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Start the validator
    Run,
    /// Key management utilities
    #[command(subcommand)]
    Keys(KeySubCmd),
}

#[derive(Args, Debug, Clone)]
pub struct Flags {
    /// Path to your private key or where you want it generated
    #[arg(long, default_value_t = DEFAULT_KEY_PATH.into())]
    pub key_path: String,
    /// Path to the folder we will keep the consensus DB
    #[arg(long, default_value_t = DEFAULT_DB_FOLDER.into())]
    pub store_path: String,
    /// Url to the engine API of the execution client
    #[arg(long, default_value_t = 8551)]
    pub engine_port: u16, // todo(dalton): should we make this URL instead of just port to support off server execution clients
    /// JWT token for communicating with engine api
    #[arg(
        long,
        default_value_t = String::from("~/.seismic/jwt.hex")
    )]
    pub engine_jwt_path: String, // todo(dalton): Lets point this at a file instead of expecting a string to keep inline with how reth handles this
    /// Port Consensus runs on
    #[arg(long, default_value_t = 8551)]
    pub port: u16,

    #[arg(long, default_value_t = 4)]
    pub worker_threads: usize,

    /// level for logs (error,warn,info,debug,trace)
    #[arg(
        long,
        default_value_t = String::from("debug")
    )]
    pub log_level: String,
    #[arg(
        long,
        default_value_t = String::from("quartz")
    )]
    pub db_prefix: String,
}

impl Command {
    pub fn exec(&self, flags: &Flags) {
        match self {
            Command::Run => self.run_node(flags),
            Command::Keys(cmd) => cmd.exec(flags),
        }
    }

    pub fn run_node(&self, flags: &Flags) {
        // todo(dalton): Handle committee needs to be standaridized and handles through flags
        let committee = GenesisCommittee::load_from_file("test_committee.toml".into());

        let engine_url = format!("http://0.0.0.0:{}", flags.engine_port);
        let peers: Vec<PublicKey> = committee.validators.iter().map(|v| v.0.clone()).collect();

        let config = EngineConfig::get_engine_config(
            engine_url,
            flags.engine_jwt_path.clone(),
            flags.key_path.clone(),
            peers.clone(),
            flags.db_prefix.clone(),
        )
        .unwrap();

        let our_ip = committee
            .ip_of(&config.signer.public_key())
            .expect("This node is not on the committee");

        // Initialize runtime
        let cfg = tokio::Config::default()
            .with_tcp_nodelay(Some(true))
            .with_worker_threads(flags.worker_threads)
            .with_storage_directory(PathBuf::from(&flags.store_path))
            .with_catch_panics(false);
        let executor = tokio::Runner::new(cfg);

        executor.start(|context| async move {
            // Configure telemetry
            let log_level = Level::from_str(&flags.log_level).expect("Invalid log level");
            tokio::telemetry::init(
                context.with_label("telemetry"),
                tokio::telemetry::Logging {
                    level: log_level,
                    // todo: dont know what this does
                    json: false,
                },
                Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 6969)),
                None,
            );

            // configure network
            let p2p_namespace = union_unique(NAMESPACE, b"_P2P");

            let mut p2p_cfg = authenticated::Config::aggressive(
                config.signer.clone(),
                &p2p_namespace,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), flags.port),
                our_ip,
                committee.validators,
                MAX_MESSAGE_SIZE,
            );
            p2p_cfg.mailbox_size = config.mailbox_size;

            // Start p2p
            let (mut network, mut oracle) =
                authenticated::Network::new(context.with_label("network"), p2p_cfg);

            // Provide authorized peers
            oracle.register(0, peers.clone()).await;

            // Register voter channel
            let voter_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
            let voter = network.register(VOTER_CHANNEL, voter_limit, MESSAGE_BACKLOG, None);

            // Register resolver channel
            let resolver_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
            let resolver =
                network.register(RESOLVER_CHANNEL, resolver_limit, MESSAGE_BACKLOG, None);

            // Register broadcast channel
            let broadcaster_limit = Quota::per_second(NonZeroU32::new(8).unwrap());
            let broadcaster = network.register(
                BROADCASTER_CHANNEL,
                broadcaster_limit,
                MESSAGE_BACKLOG,
                Some(3),
            );

            let backfiller = network.register(
                BACKFILLER_CHANNEL,
                config.backfill_quota,
                MESSAGE_BACKLOG,
                Some(3),
            );

            // Create network
            let p2p = network.start();
            // create engine
            let engine = Engine::new(context.with_label("engine"), config).await;

            // Start engine
            let engine = engine.start(voter, resolver, broadcaster, backfiller);

            // Wait for any task to error
            if let Err(e) = try_join_all(vec![p2p, engine]).await {
                error!(?e, "task failed");
            }
        })
    }
}

pub fn run_node_with_runtime(
    context: commonware_runtime::tokio::Context,
    flags: Flags,
) -> Handle<()> {
    // todo(dalton): Handle committee needs to be standaridized and handles through flags
    let committee = GenesisCommittee::load_from_file("test_committee.toml".into());

    let engine_url = format!("http://0.0.0.0:{}", flags.engine_port);
    let peers: Vec<PublicKey> = committee.validators.iter().map(|v| v.0.clone()).collect();

    let config = EngineConfig::get_engine_config(
        engine_url,
        flags.engine_jwt_path.clone(),
        flags.key_path.clone(),
        peers.clone(),
        flags.db_prefix.clone(),
    )
    .unwrap();

    let our_ip = committee
        .ip_of(&config.signer.public_key())
        .expect("This node is not on the committee");

    context.spawn(async move |context| {
        // configure network
        let p2p_namespace = union_unique(NAMESPACE, b"_P2P");

        let mut p2p_cfg = authenticated::Config::aggressive(
            config.signer.clone(),
            &p2p_namespace,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), flags.port),
            our_ip,
            committee.validators,
            MAX_MESSAGE_SIZE,
        );
        p2p_cfg.mailbox_size = config.mailbox_size;

        // Start p2p
        let (mut network, mut oracle) =
            authenticated::Network::new(context.with_label("network"), p2p_cfg);

        // Provide authorized peers
        oracle.register(0, peers.clone()).await;

        // Register voter channel
        let voter_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let voter = network.register(VOTER_CHANNEL, voter_limit, MESSAGE_BACKLOG, None);

        // Register resolver channel
        let resolver_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let resolver = network.register(RESOLVER_CHANNEL, resolver_limit, MESSAGE_BACKLOG, None);

        // Register broadcast channel
        let broadcaster_limit = Quota::per_second(NonZeroU32::new(8).unwrap());
        let broadcaster = network.register(
            BROADCASTER_CHANNEL,
            broadcaster_limit,
            MESSAGE_BACKLOG,
            Some(3),
        );

        let backfiller = network.register(
            BACKFILLER_CHANNEL,
            config.backfill_quota,
            MESSAGE_BACKLOG,
            Some(3),
        );

        // Create network
        let p2p = network.start();
        // create engine
        let engine = Engine::new(context.with_label("engine"), config).await;

        // Start engine
        let engine = engine.start(voter, resolver, broadcaster, backfiller);

        // Wait for any task to error
        if let Err(e) = try_join_all(vec![p2p, engine]).await {
            error!(?e, "task failed");
        }
    })
}

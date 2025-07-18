use seismicbft_types::PublicKey;

#[derive(Clone)]
pub struct ApplicationConfig {
    /// Participants active in consensus.
    pub participants: Vec<PublicKey>,

    /// Number of messages from consensus to hold in our backlog
    /// before blocking.
    pub mailbox_size: usize,

    /// Url to the engine api on Seismic Reth
    pub engine_url: String,

    /// Shared jwt auth key for Seismic Reth engine api
    pub engine_jwt: String,

    pub partition_prefix: String,
}

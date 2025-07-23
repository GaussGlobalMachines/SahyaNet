/*
This is the Client to speak with the engine API on Reth

The engine api is what consensus uses to drive the execution client forward. There is only 3 main endpoints that we hit
but they do different things depending on the args

engine_forkchoiceUpdatedV3 : This updates the forkchoice head to a specific head. If the optionally arg payload_attributes is provided it will also trigger the
    building of a new block on the execution client. This will mainly be called in 2 scenerios: 1) When a validator has been selected to propose a block he will
    call with payload_attributes to trigger the building process. 2) After a block a validator has previously validated a block(therefore saved on execution client) and
    it has received enough attestations to be committed by consensus


engine_getPayloadV3 : This is called to retrieve a block from execution client. This is called after a node has previously called engine_forkchoiceUpdatedV3 with payload
    attributes to begin the build process

engine_newPayloadV3 : This is called to store(not commit) and validate blocks received from other validators. This is called after receiving a block and it is how we decide if
    we should attest if the block is valid. If it is valid and we reach quorom when we call engine_forkchoiceUpdatedV3 it will set this block to head

*/
use alloy_provider::{RootProvider, ext::EngineApi, network::Ethereum};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV4, ForkchoiceState, JwtSecret, PayloadAttributes, PayloadId,
    PayloadStatus,
};
use alloy_transport_http::{
    AuthLayer, AuthService, Http, HyperClient,
    hyper::body::Bytes as HyperBytes,
    hyper_util::{client::legacy::Client, rt::TokioExecutor},
};
use http_body_util::Full;
use seismicbft_types::Block;

/// The list of all supported Engine capabilities available over the engine endpoint.
///
/// Latest spec: Prague
pub const CAPABILITIES: &[&str] = &[
    "engine_forkchoiceUpdatedV1",
    "engine_forkchoiceUpdatedV2",
    "engine_forkchoiceUpdatedV3",
    "engine_exchangeTransitionConfigurationV1",
    "engine_getClientVersionV1",
    "engine_getPayloadV1",
    "engine_getPayloadV2",
    "engine_getPayloadV3",
    "engine_getPayloadV4",
    "engine_newPayloadV1",
    "engine_newPayloadV2",
    "engine_newPayloadV3",
    "engine_newPayloadV4",
    "engine_getPayloadBodiesByHashV1",
    "engine_getPayloadBodiesByRangeV1",
];

#[derive(Clone)]
pub struct EngineClient {
    provider: RootProvider,
}

impl EngineClient {
    pub fn new(engine_url: String, jwt_secret: &str) -> Self {
        let secret = JwtSecret::from_hex(jwt_secret).unwrap();
        let url = engine_url.parse().unwrap();

        // todo(dalton): bringing in Full here as a conveniance at the moment. If i dont end up using any of the benefits here we can switch to just Bytes and drop dep
        let hyper_client = Client::builder(TokioExecutor::new()).build_http::<Full<HyperBytes>>();
        let service = tower::ServiceBuilder::new()
            .layer(AuthLayer::new(secret))
            .service(hyper_client);

        let layer_transport: HyperClient<
            Full<HyperBytes>,
            AuthService<
                Client<
                    alloy_transport_http::hyper_util::client::legacy::connect::HttpConnector,
                    Full<HyperBytes>,
                >,
            >,
        > = HyperClient::with_service(service);

        let http_hyper = Http::with_client(layer_transport, url);

        let rpc_client = alloy_rpc_client::RpcClient::new(http_hyper, true);

        let provider = RootProvider::<Ethereum>::new(rpc_client);

        Self { provider }
    }
}

impl EngineClient {
    pub async fn start_building_block(
        &self,
        fork_choice_state: ForkchoiceState,
        timestamp: u64,
    ) -> Option<PayloadId> {
        let payload_attributes = PayloadAttributes {
            timestamp,
            prev_randao: [0; 32].into(),
            // todo(dalton): this should be the validators public key
            suggested_fee_recipient: [1; 20].into(),
            withdrawals: Some(Vec::new()),
            // todo(dalton): we should make this something that we can associate with the simplex height
            parent_beacon_block_root: Some([1; 32].into()),
        };
        let res = self
            .provider
            .fork_choice_updated_v3(fork_choice_state, Some(payload_attributes))
            .await
            .unwrap();

        res.payload_id
    }

    pub async fn get_payload(&self, payload_id: PayloadId) -> ExecutionPayloadEnvelopeV4 {
        self.provider.get_payload_v4(payload_id).await.unwrap()
    }

    pub async fn check_payload(&self, block: &Block) -> PayloadStatus {
        self.provider
            .new_payload_v4(
                block.payload.clone(),
                Vec::new(),
                [1; 32].into(),
                block.execution_requests.clone(),
            )
            .await
            .unwrap()
    }

    pub async fn commit_hash(&self, fork_choice_state: ForkchoiceState) {
        self.provider
            .fork_choice_updated_v3(fork_choice_state, None)
            .await
            .unwrap();
    }
}

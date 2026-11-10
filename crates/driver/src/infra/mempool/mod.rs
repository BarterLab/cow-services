use {
    crate::{
        boundary::{Web3, unbuffered_web3},
        domain::mempools,
        infra::{self, solver::Account},
    },
    alloy::{
        consensus::Transaction,
        eips::{BlockNumberOrTag, eip1559::Eip1559Estimation, eip2718::Encodable2718},
        network::{EthereumWallet, NetworkTransactionBuilder, TxSigner},
        primitives::Address,
        providers::{
            Provider,
            ext::{TxPoolApi, sign_flashbots_payload},
        },
        rpc::types::TransactionRequest,
        transports::TransportError,
    },
    anyhow::Context,
    dashmap::DashMap,
    eth_domain_types as eth,
    std::{ops::RangeInclusive, sync::Arc},
    url::Url,
};

#[derive(Debug, Clone)]
pub struct Config {
    pub target_confirm_time: std::time::Duration,
    pub retry_interval: std::time::Duration,
    /// Optional block number to use when fetching nonces. If None, uses the
    /// web3 lib's default behavior, which is `latest`.
    pub nonce_block_number: Option<BlockNumberOrTag>,
    pub url: Url,
    pub name: String,
    pub revert_protection: RevertProtection,
}

/// Don't submit transactions with high revert risk (i.e. transactions
/// that interact with on-chain AMMs) to the public mempool.
/// This can be enabled to avoid MEV when private transaction
/// submission strategies are available. If private submission strategies
/// are not available, revert protection is always disabled.
#[derive(Debug, Clone, Copy)]
pub enum RevertProtection {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Mempool {
    transport: Web3,
    config: Config,
    last_submissions: Arc<DashMap<Address, Submission>>,
    relay_client: reqwest::Client,
}

pub struct RelaySubmission<'a> {
    pub account: &'a Account,
    pub nonce: u64,
    pub chain_id: u64,
    pub blocks: RangeInclusive<u64>,
    pub exact_timestamp: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Submission {
    pub nonce: u64,
    pub gas_price: Eip1559Estimation,
}

impl std::fmt::Display for Mempool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mempool({})", self.config.name)
    }
}

impl Mempool {
    pub fn new(config: Config, solver_accounts: Vec<Account>) -> Self {
        let transport = unbuffered_web3(&config.url);
        // Register the solver accounts into the wallet to submit txs on their
        // behalf
        for account in solver_accounts {
            transport.wallet.register_signer(account);
        }
        Self {
            transport,
            config,
            last_submissions: Default::default(),
            relay_client: reqwest::Client::new(),
        }
    }

    /// Fetches the transaction count (nonce) for the given address at the
    /// specified block number. If no block number is provided in the config,
    /// uses the alloy's default behavior.
    pub async fn get_nonce(&self, address: eth::Address) -> Result<u64, mempools::Error> {
        get_nonce(
            &self.transport.provider,
            address,
            self.config.nonce_block_number,
        )
        .await
        .map_err(mempools::Error::Other)
    }

    /// Submits a transaction to the mempool. Returns optimistically as soon as
    /// the transaction is pending. `signer` is the address that signs and pays
    /// for gas (may differ from the solver address in EIP-7702 mode).
    pub async fn submit(
        &self,
        tx: eth::Tx,
        gas_price: Eip1559Estimation,
        gas_limit: eth::Gas,
        signer: eth::Address,
        nonce: u64,
    ) -> Result<eth::TxId, mempools::Error> {
        let gas_limit = gas_limit.0.try_into().map_err(anyhow::Error::from)?;
        let tx_request = tx_request(tx, gas_price, gas_limit, signer, nonce);

        let submission = self
            .transport
            .provider
            .send_transaction(tx_request)
            .await
            .inspect_err(log_rpc_error_code)
            .map_err(anyhow::Error::from);

        match submission {
            Ok(tx) => {
                tracing::debug!(
                    ?nonce,
                    ?gas_price,
                    ?gas_limit,
                    ?signer,
                    "successfully submitted tx to mempool"
                );
                self.last_submissions
                    .insert(signer, Submission { nonce, gas_price });
                Ok(eth::TxId(*tx.tx_hash()))
            }
            Err(err) => {
                // log pending tx in case we failed to replace a pending tx
                let last_submission = self.last_submission(signer);

                tracing::debug!(
                    ?err,
                    new_gas_price = ?gas_price,
                    ?nonce,
                    ?last_submission,
                    ?gas_limit,
                    ?signer,
                    "failed to submit tx to mempool"
                );
                Err(mempools::Error::Other(err))
            }
        }
    }

    /// Submit the same signed transaction to a relay for each requested block.
    pub async fn submit_till_block(
        &self,
        tx: eth::Tx,
        gas_price: Eip1559Estimation,
        gas_limit: eth::Gas,
        relay: RelaySubmission<'_>,
    ) -> Result<eth::TxId, mempools::Error> {
        if relay.blocks.is_empty() {
            return Err(anyhow::anyhow!("no block left before the deadline").into());
        }
        let account = relay.account;
        let signer = TxSigner::address(account);
        let nonce = relay.nonce;
        let mut request = tx_request(
            tx,
            gas_price,
            gas_limit.0.try_into().map_err(anyhow::Error::from)?,
            signer,
            nonce,
        );
        request.chain_id = Some(relay.chain_id);
        let envelope = request
            .build(&EthereumWallet::from(account.clone()))
            .await
            .context("failed to sign relay transaction")?;
        let hash = eth::TxId(*envelope.tx_hash());
        let raw = alloy::primitives::Bytes::from(envelope.encoded_2718());
        let mut last_error = None;
        let mut accepted = false;
        for block in relay.blocks {
            let mut bundle = serde_json::json!({
                "txs": [&raw], "blockNumber": format!("0x{block:x}"), "droppingTxHashes": [hash.0]
            });
            if let Some(timestamp) = relay.exact_timestamp {
                bundle["minTimestamp"] = timestamp.into();
                bundle["maxTimestamp"] = timestamp.into();
            }
            let body = serde_json::to_string(&serde_json::json!({
                "id": 1, "jsonrpc": "2.0", "method": "eth_sendBundle", "params": [bundle]
            }))
            .map_err(anyhow::Error::from)?;
            let signature = sign_flashbots_payload(body.clone(), account)
                .await
                .context("sign relay request")?;
            if let Some(timestamp) = relay.exact_timestamp {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("invalid system time")?;
                if now.as_secs() >= timestamp {
                    return Err(
                        anyhow::anyhow!("exact relay bundle expired before submission").into(),
                    );
                }
            }
            let result = async {
                let response = self
                    .relay_client
                    .post(self.config.url.clone())
                    .timeout(std::time::Duration::from_secs(3))
                    .header("Content-Type", "application/json")
                    .header("X-Flashbots-Signature", signature)
                    .body(body)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<serde_json::Value>()
                    .await?;
                if let Some(error) = response.get("error") {
                    anyhow::bail!("relay rejected bundle: {error}");
                }
                anyhow::ensure!(
                    response
                        .get("result")
                        .is_some_and(|result| !result.is_null()),
                    "missing bundle result"
                );
                Ok::<(), anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => accepted = true,
                Err(error) => last_error = Some(error),
            }
        }
        if !accepted {
            return Err(last_error
                .unwrap_or_else(|| anyhow::anyhow!("no bundle accepted"))
                .into());
        }
        self.last_submissions
            .insert(signer, Submission { nonce, gas_price });
        Ok(hash)
    }

    /// Queries the mempool for a pending transaction of the given solver and
    /// nonce.
    pub async fn find_pending_tx_in_mempool(
        &self,
        signer: eth::Address,
        nonce: u64,
    ) -> anyhow::Result<Option<alloy::rpc::types::Transaction>> {
        let tx_pool_content = self
            .transport
            .provider
            .txpool_content_from(signer)
            .await
            .context("failed to query pending transactions")?;

        // find the one with the specified nonce
        let pending_tx = tx_pool_content
            .pending
            .into_iter()
            .chain(tx_pool_content.queued)
            .find(|(_signer, tx)| tx.nonce() == nonce)
            .map(|(_, tx)| tx);
        Ok(pending_tx)
    }

    /// Looks up the last tx that was submitted for that signer.
    pub fn last_submission(&self, signer: eth::Address) -> Option<Submission> {
        self.last_submissions
            .get(&signer)
            .map(|entry| entry.value().clone())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn reverts_can_get_mined(&self) -> bool {
        matches!(
            self.config.revert_protection,
            infra::mempool::RevertProtection::Disabled
        )
    }
}

/// Fetches the transaction count (nonce) of `address` at `block`, or at
/// alloy's default (`latest`) when `None`.
pub async fn get_nonce(
    provider: &impl Provider,
    address: eth::Address,
    block: Option<BlockNumberOrTag>,
) -> anyhow::Result<u64> {
    let call = provider.get_transaction_count(address);
    match block {
        Some(BlockNumberOrTag::Latest) => call.latest(),
        Some(BlockNumberOrTag::Earliest) => call.earliest(),
        Some(BlockNumberOrTag::Finalized) => call.finalized(),
        Some(BlockNumberOrTag::Number(number)) => call.number(number),
        Some(BlockNumberOrTag::Pending) => call.pending(),
        Some(BlockNumberOrTag::Safe) => call.safe(),
        None => call,
    }
    .await
    .context("failed to fetch nonce")
}

/// Builds the EIP-1559 transaction request for `tx`.
pub fn tx_request(
    tx: eth::Tx,
    gas_price: Eip1559Estimation,
    gas_limit: u64,
    signer: eth::Address,
    nonce: u64,
) -> TransactionRequest {
    TransactionRequest::default()
        .from(signer)
        .to(tx.to)
        .nonce(nonce)
        .max_fee_per_gas(gas_price.max_fee_per_gas)
        .max_priority_fee_per_gas(gas_price.max_priority_fee_per_gas)
        .gas_limit(gas_limit)
        .input(tx.input.into())
        .value(tx.value.0)
        .access_list(tx.access_list.into())
}

/// Logs a hint for known JSON-RPC error codes. Codes in -32768..=-32000 mean
/// different things per chain, so only map new codes once they're understood.
fn log_rpc_error_code(err: &TransportError) {
    match err.as_error_resp().map(|resp| resp.code) {
        // Standard error, e.g. "replacement transaction underpriced".
        None | Some(-32000) => {}
        // A proxy in front of the sequencer rejected the tx (seen on Ink).
        // https://github.com/ethereum-optimism/infra/blob/a4d73e6c8afed6b2dcb007d1c6f26455df45599f/proxyd/backend.go#L140-L144
        Some(-32100) => tracing::warn!(
            "transaction rejected upstream (JSON-RPC -32100): the RPC's proxy refused it before \
             the sequencer"
        ),
        Some(code) => tracing::warn!("unexpected error code {code}"),
    }
}
#[cfg(test)]
mod relay_tests {
    use {
        super::*,
        alloy::{
            consensus::TxEnvelope,
            eips::eip2718::Decodable2718,
            primitives::Bytes,
            providers::ext::verify_flashbots_signature,
            signers::local::PrivateKeySigner,
        },
        axum::{
            Json,
            Router,
            body::Bytes as BodyBytes,
            extract::State,
            http::HeaderMap,
            routing::post,
        },
        serde_json::{Value, json},
        std::sync::Mutex,
    };

    type Requests = Arc<Mutex<Vec<(Address, Value)>>>;

    async fn accept(
        State(requests): State<Requests>,
        headers: HeaderMap,
        body: BodyBytes,
    ) -> Json<Value> {
        let signer =
            verify_flashbots_signature(headers["X-Flashbots-Signature"].to_str().unwrap(), &body)
                .unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let id = request["id"].clone();
        requests.lock().unwrap().push((signer, request));
        Json(json!({"jsonrpc":"2.0", "id": id, "result": {"bundleHash": "accepted"}}))
    }

    #[tokio::test]
    async fn exact_relay_bundle_uses_reserved_nonce_chain_and_timestamp() {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap())
            .parse()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/", post(accept))
            .with_state(requests.clone());
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let account = Account::PrivateKey(PrivateKeySigner::random());
        let from = TxSigner::address(&account);
        let relay = Mempool::new(
            Config {
                target_confirm_time: Default::default(),
                retry_interval: Default::default(),
                nonce_block_number: None,
                name: "relay".into(),
                url,
                revert_protection: RevertProtection::Enabled,
            },
            vec![account.clone()],
        );
        let target_timestamp = 5_000_000_000;
        let hash = relay
            .submit_till_block(
                eth::Tx {
                    from,
                    to: Address::repeat_byte(2),
                    value: 0.into(),
                    input: Bytes::new(),
                    access_list: Default::default(),
                },
                Eip1559Estimation {
                    max_fee_per_gas: 100,
                    max_priority_fee_per_gas: 10,
                },
                50_000u64.into(),
                RelaySubmission {
                    account: &account,
                    nonce: 19,
                    chain_id: 8453,
                    blocks: 23..=23,
                    exact_timestamp: Some(target_timestamp),
                },
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        let [(signer, request)] = requests.as_slice() else {
            panic!("expected exactly one bundle submission");
        };
        assert_eq!(*signer, from);
        assert_eq!(request["method"], "eth_sendBundle");
        let bundle = &request["params"][0];
        assert_eq!(bundle["blockNumber"], "0x17");
        assert_eq!(bundle["minTimestamp"], target_timestamp);
        assert_eq!(bundle["maxTimestamp"], target_timestamp);
        assert_eq!(bundle["droppingTxHashes"], json!([hash.0]));
        let raw: Bytes = serde_json::from_value(bundle["txs"][0].clone()).unwrap();
        let signed = TxEnvelope::decode_2718_exact(raw.as_ref()).unwrap();
        assert_eq!(*signed.tx_hash(), hash.0);
        assert_eq!(signed.chain_id(), Some(8453));
        assert_eq!(signed.nonce(), 19);
        assert_eq!(signed.gas_limit(), 50_000);
        server.abort();
    }
}

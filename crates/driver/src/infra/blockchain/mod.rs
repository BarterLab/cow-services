use {
    crate::{
        boundary,
        domain::{blockchain::TxStatus, competition::solution::SimulationContext},
        infra::{blockchain::gas::GasPriceParameters, config::file::GasEstimatorType},
    },
    account_balances::{BalanceSimulator, SimulationError},
    alloy::{
        eips::{BlockId, BlockNumberOrTag, eip1559::Eip1559Estimation},
        network::TransactionBuilder,
        providers::Provider,
        rpc::types::{TransactionReceipt, TransactionRequest, state::StateOverride},
        transports::TransportErrorKind,
    },
    anyhow::anyhow,
    balance_overrides::{StateOverrides, StateOverriding},
    chain::Chain,
    eth_domain_types as eth,
    ethrpc::{Web3, alloy::ProviderLabelingExt, block_stream::CurrentBlockWatcher},
    gas_price_estimation::{Eip1559EstimationExt, GasPriceEstimating},
    shared::web3,
    solvers_dto::solution::StateOverrideBlock,
    std::{
        fmt,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    },
    thiserror::Error,
    tracing::{Level, instrument},
    url::Url,
};

pub mod contracts;
pub mod gas;
pub mod token;
pub use self::{contracts::Contracts, gas::GasPriceEstimator};

/// An Ethereum RPC connection.
#[derive(Clone)]
pub struct Rpc {
    web3: Web3,
    chain: Chain,
    args: RpcArgs,
}

#[derive(Clone)]
pub struct RpcArgs {
    pub url: Url,
    pub max_batch_size: usize,
    pub max_concurrent_requests: usize,
}

impl Rpc {
    /// Instantiate an RPC client to an Ethereum (or Ethereum-compatible) node
    /// at the specifed URL.
    pub async fn try_new(args: RpcArgs) -> Result<Self, RpcError> {
        let web3 = web3::web3(
            &web3::Arguments {
                ethrpc_max_batch_size: args.max_batch_size,
                ethrpc_max_concurrent_requests: args.max_concurrent_requests,
                ethrpc_batch_delay: Default::default(),
            },
            &args.url,
            "base",
        );
        let chain = Chain::try_from(web3.provider.get_chain_id().await?)?;

        Ok(Self { web3, chain, args })
    }

    /// Returns the chain for the RPC connection.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Returns a reference to the underlying web3 client.
    pub fn web3(&self) -> &Web3 {
        &self.web3
    }
}

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("alloy transport error: {0:?}")]
    Alloy(#[from] alloy::transports::TransportError),
    #[error("unsupported chain")]
    UnsupportedChain(#[from] chain::ChainIdNotSupported),
}

fn state_override_block_param(block: Option<StateOverrideBlock>) -> BlockId {
    match block {
        Some(StateOverrideBlock::Number(number)) => BlockId::number(number),
        Some(StateOverrideBlock::Pending) => BlockId::pending(),
        None => BlockId::latest(),
    }
}

/// The Ethereum blockchain.
#[derive(Clone)]
pub struct Ethereum {
    web3: Web3,
    inner: Arc<Inner>,
}

struct Inner {
    chain: Chain,
    contracts: Contracts,
    gas: Arc<GasPriceEstimator>,
    current_block: CurrentBlockWatcher,
    balance_simulator: BalanceSimulator,
    state_overrider: Arc<dyn StateOverriding>,
    tx_gas_limit: eth::Gas,
}

impl Ethereum {
    /// Access the Ethereum blockchain through an RPC API.
    ///
    /// # Panics
    ///
    /// Since this type is essential for the program this method will panic on
    /// any initialization error.
    pub async fn new(
        rpc: Rpc,
        addresses: contracts::Addresses,
        current_block_args: &shared::current_block::Arguments,
        tx_gas_limit: eth::Gas,
        gas_estimator_type: &GasEstimatorType,
        gas_adjustments: GasPriceParameters,
    ) -> Self {
        Self::new_from(
            rpc,
            addresses,
            current_block_args,
            tx_gas_limit,
            gas_estimator_type,
            gas_adjustments,
            None,
        )
        .await
    }

    pub async fn new_from(
        rpc: Rpc,
        addresses: contracts::Addresses,
        current_block_args: &shared::current_block::Arguments,
        tx_gas_limit: eth::Gas,
        gas_estimator_type: &GasEstimatorType,
        gas_adjustments: GasPriceParameters,
        from_address: Option<eth::Address>,
    ) -> Self {
        let Rpc { web3, chain, args } = rpc;
        let current_block_stream = current_block_args
            .stream(args.url.clone(), web3.provider.clone())
            .await
            .expect("couldn't initialize current block stream");

        let gas = Arc::new(
            GasPriceEstimator::new(
                &web3.provider,
                &current_block_stream,
                gas_estimator_type,
                gas_adjustments,
            )
            .await
            .expect("initialize gas price estimator"),
        );

        let contracts = Contracts::new(&web3, chain, addresses, from_address)
            .await
            .expect("could not initialize important smart contracts");
        let state_overrider = Arc::new(StateOverrides::new(web3.clone()));
        let balance_simulator = BalanceSimulator::new(
            contracts.settlement().clone(),
            contracts.balance_helper().clone(),
            *contracts.vault_relayer(),
            state_overrider.clone(),
        );

        Self {
            inner: Arc::new(Inner {
                current_block: current_block_stream,
                chain,
                contracts,
                gas,
                balance_simulator,
                state_overrider,
                tx_gas_limit,
            }),
            web3,
        }
    }

    pub fn chain(&self) -> Chain {
        self.inner.chain
    }

    pub fn balance_simulator(&self) -> &BalanceSimulator {
        &self.inner.balance_simulator
    }

    pub fn state_overrider(&self) -> Arc<dyn StateOverriding> {
        Arc::clone(&self.inner.state_overrider)
    }

    pub fn balance_overrider(&self) -> Arc<dyn StateOverriding> {
        self.state_overrider()
    }

    /// Clones self and returns an instance that captures metrics extended with
    /// the provided label.
    pub fn with_metric_label(&self, label: String) -> Self {
        Self {
            web3: self.web3.labeled(label),
            ..self.clone()
        }
    }

    /// Onchain smart contract bindings.
    pub fn contracts(&self) -> &Contracts {
        &self.inner.contracts
    }

    /// Check if a smart contract is deployed to the given address.
    pub async fn is_contract(&self, address: eth::Address) -> Result<bool, Error> {
        let code = self.web3.provider.get_code_at(address).await?;
        Ok(!code.is_empty())
    }

    /// Returns a type that monitors the block chain to inform about the current
    /// block.
    pub fn current_block(&self) -> &CurrentBlockWatcher {
        &self.inner.current_block
    }

    /// Fee of the next inclusion block, computed from an already observed head.
    pub fn inclusion_base_fee(&self, head: &ethrpc::block_stream::BlockInfo) -> u64 {
        if self.chain() == Chain::Mainnet { head.next_base_fee } else { head.base_fee }
    }

    /// The gas price is determined based on the deadline by which the
    /// transaction must be included on-chain. A shorter deadline requires a
    /// higher gas price to increase the likelihood of timely inclusion.
    pub async fn gas_price(&self) -> Result<Eip1559Estimation, Error> {
        self.inner.gas.estimate().await
    }

    pub fn gas_estimator(&self) -> Arc<dyn GasPriceEstimating> {
        Arc::clone(&self.inner.gas) as _
    }

    pub fn block_gas_limit(&self) -> eth::Gas {
        self.inner.current_block.borrow().gas_limit.into()
    }

    /// Per-transaction gas limit configured for this driver. Operators set
    /// this per chain (e.g. EIP-7825's 16,777,215 cap on Fusaka chains).
    pub fn tx_gas_limit(&self) -> eth::Gas {
        self.inner.tx_gas_limit
    }

    /// Returns the current [`eth::Ether`] balance of the specified account.
    pub async fn balance(&self, address: eth::Address) -> Result<eth::Ether, Error> {
        self.web3
            .provider
            .get_balance(address)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Returns a [`token::Erc20`] for the specified address.
    pub fn erc20(&self, address: eth::TokenAddress) -> token::Erc20 {
        token::Erc20::new(self, address)
    }

    /// Create access list used by a transaction.
    #[instrument(skip_all)]
    pub async fn create_access_list<T>(&self, tx: T) -> Result<eth::AccessList, Error>
    where
        T: Into<TransactionRequest>,
    {
        self.create_access_list_with_state_overrides(tx, None, None)
            .await
    }

    pub async fn create_access_list_with_state_overrides<T>(
        &self,
        tx: T,
        state_overrides: Option<&StateOverride>,
        state_overrides_block: Option<StateOverrideBlock>,
    ) -> Result<eth::AccessList, Error>
    where
        T: Into<TransactionRequest>,
    {
        let tx = tx.into();

        let gas_limit = match self.chain() {
            Chain::Mainnet => 2_000_000,
            _ => self.inner.tx_gas_limit.0.try_into().map_err(|err| {
                Error::GasPrice(anyhow!("failed to convert gas_limit to u64: {err:?}"))
            })?,
        };
        let gas_limit = tx
            .gas
            .unwrap_or(gas_limit)
            .min(gas_limit)
            .min(self.simulation_gas_limit()?);
        let tx = tx.with_gas_limit(gas_limit);
        let tx = match self.simulation_gas_price().await {
            Some(gas_price) => tx.with_gas_price(gas_price),
            _ => tx,
        };

        let access_list = match state_overrides {
            Some(state_overrides) => {
                let params = serde_json::json!([
                    tx,
                    state_override_block_param(state_overrides_block),
                    state_overrides,
                ]);
                self.web3
                    .provider
                    .raw_request("eth_createAccessList".into(), params)
                    .await?
            }
            None => {
                self.web3
                    .provider
                    .create_access_list(&tx)
                    .block_id(state_override_block_param(state_overrides_block))
                    .await?
            }
        };

        let access_list = access_list.ensure_ok().map_err(Error::AccessList)?;

        Ok(access_list.access_list.into())
    }

    /// Estimate gas used by a transaction.
    pub async fn estimate_gas(&self, tx: eth::Tx) -> Result<eth::Gas, Error> {
        self.estimate_gas_with_state_overrides(tx, None, None).await
    }

    pub async fn estimate_gas_with_state_overrides(
        &self,
        tx: eth::Tx,
        state_overrides: Option<&StateOverride>,
        state_overrides_block: Option<StateOverrideBlock>,
    ) -> Result<eth::Gas, Error> {
        let tx = self.simulation_transaction(tx, None).await?;

        let estimate = self
            .web3
            .provider
            .estimate_gas(tx)
            .overrides_opt(state_overrides.cloned());

        let estimated_gas = estimate
            .block(state_override_block_param(state_overrides_block))
            .await
            .map_err(Error::Rpc)?
            .into();

        Ok(estimated_gas)
    }

    /// Estimate gas against the canonical base block and virtual target header.
    pub async fn estimate_gas_with_simulation_context(
        &self,
        tx: eth::Tx,
        context: &SimulationContext,
    ) -> Result<eth::Gas, Error> {
        let base = self.checked_simulation_context_block(context).await?;
        let base_fee = if self.chain() == Chain::Mainnet {
            Some(base.header.next_block_base_fee(alloy::eips::eip1559::BaseFeeParams::ethereum())
                .ok_or_else(|| Error::GasPrice(anyhow!("no next-block base fee available")))?)
        } else {
            None
        };
        let block_overrides = context.block_overrides(base_fee);
        let tx = self.simulation_transaction(tx, base_fee).await?;
        let gas: alloy::primitives::U64 = self
            .web3
            .provider
            .raw_request("eth_estimateGas".into(), (
                &tx, context.base_block_id(), context.state_overrides(), &block_overrides,
            ))
            .await
            .map_err(|error| {
                tracing::error!(
                    rpc_method = "eth_estimateGas",
                    base_block_number = context.base_number(),
                    rpc_params = %serde_json::json!([
                        tx,
                        context.base_block_id(),
                        context.state_overrides(),
                        block_overrides,
                    ]),
                    rpc_error = ?error.as_error_resp(),
                    "Simulation context RPC failed",
                );
                Error::Rpc(error)
            })?;

        self.ensure_simulation_context_current(context).await?;
        Ok(gas.to::<u64>().into())
    }

    /// Ensure a one-block simulation lease still points at the canonical head.
    pub async fn ensure_simulation_context_current(
        &self,
        context: &SimulationContext,
    ) -> Result<(), Error> {
        self.checked_simulation_context_block(context).await.map(|_| ())
    }

    async fn checked_simulation_context_block(
        &self,
        context: &SimulationContext,
    ) -> Result<alloy::rpc::types::Block, Error> {
        let latest = self
            .web3
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(Error::Rpc)?
            .ok_or(Error::LatestBlockUnavailable)?;
        if latest.header.number != context.base_number()
            || latest.header.hash != context.base_hash()
        {
            return Err(Error::StaleSimulationContext {
                expected_number: context.base_number(),
                expected_hash: context.base_hash(),
                actual_number: latest.header.number,
                actual_hash: latest.header.hash,
            });
        }
        if context.target_timestamp() <= latest.header.timestamp {
            return Err(Error::InvalidTargetTimestamp {
                base_timestamp: latest.header.timestamp,
                target_timestamp: context.target_timestamp(),
            });
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| Error::SystemClock(error.to_string()))?
            .as_secs();
        if context.target_timestamp() <= now {
            return Err(Error::ExpiredTargetTimestamp {
                current_timestamp: now,
                target_timestamp: context.target_timestamp(),
            });
        }

        Ok(latest)
    }

    fn simulation_gas_limit(&self) -> Result<u64, Error> {
        self.tx_gas_limit()
            .0
            .try_into()
            .map_err(|err| Error::GasPrice(anyhow!("invalid transaction gas limit: {err}")))
    }

    async fn simulation_transaction(&self, tx: eth::Tx, base_fee: Option<u64>) -> Result<TransactionRequest, Error> {
        let tx = TransactionRequest::default()
            .with_gas_limit(self.simulation_gas_limit()?)
            .from(tx.from)
            .to(tx.to)
            .value(tx.value.0)
            .input(tx.input.into())
            .access_list(tx.access_list.into());

        let gas_price = match base_fee {
            Some(base_fee) => Some(self.gas_price().await?.effective(base_fee)),
            None => self.simulation_gas_price().await,
        };
        Ok(match gas_price {
            Some(gas_price) => tx.with_gas_price(gas_price),
            None => tx,
        })
    }

    pub async fn trace(&self, tx: &eth::Tx) -> Result<eth::Gas, Error> {
        let tx = self.simulation_transaction(tx.clone(), None).await?;
        simulator::ethereum::trace_gas(&self.web3, tx)
            .await
            .map_err(|err| match err {
                simulator::ethereum::Error::Rpc(err) => Error::Rpc(err),
                _ => Error::Trace,
            })
    }

    /// Returns the transaction's on-chain inclusion status.
    pub async fn transaction_status(&self, tx_hash: &eth::TxId) -> Result<TxStatus, Error> {
        self.web3
            .provider
            .get_transaction_receipt(tx_hash.0)
            .await
            .map(|result| {
                let Some(
                    receipt @ TransactionReceipt {
                        block_number: Some(block_number),
                        ..
                    },
                ) = result
                else {
                    return TxStatus::Pending;
                };

                if receipt.status() {
                    TxStatus::Executed {
                        block_number: eth::BlockNo(block_number),
                    }
                } else {
                    TxStatus::Reverted {
                        block_number: eth::BlockNo(block_number),
                    }
                }
            })
            .map_err(Into::into)
    }

    /// The block our successful `Settlement` event for `tx_hash` landed in,
    /// searching `[from_block, to_block]` inclusive, or `None` if it is not in
    /// that range. Only a successful settle emits the event, and `eth_getLogs`
    /// reads block logs even while the receipt-by-hash lookup still lags, so
    /// this sees a freshly mined settlement that `eth_getTransactionReceipt`
    /// would still miss. Scanning a range rather than a single block means a
    /// block the (coalescing) block stream skipped on a fast chain is still
    /// covered.
    pub async fn successful_settlement_block(
        &self,
        tx_hash: eth::TxId,
        from_block: u64,
    ) -> Result<Option<eth::BlockNo>, Error> {
        let logs = self
            .contracts()
            .settlement()
            .event_filter::<::contracts::GPv2Settlement::GPv2Settlement::Settlement>()
            .from_block(alloy::eips::BlockNumberOrTag::Number(from_block))
            // `Latest` rather than the stream's head: the node may already be a block
            // or two ahead, and querying up to its real tip narrows the race window.
            .to_block(alloy::eips::BlockNumberOrTag::Latest)
            .query()
            .await?;
        Ok(logs
            .into_iter()
            .find(|(_, log)| log.transaction_hash == Some(tx_hash.0))
            .and_then(|(_, log)| log.block_number)
            .map(eth::BlockNo))
    }

    /// The signer's transaction count including the mempool (the `pending`
    /// tag). A count above the nonce we submitted with means our tx is
    /// still queued in the mempool or has already mined (the count stays at
    /// `nonce + 1` once it mines). A count equal to that nonce means the tx
    /// left the mempool without mining.
    pub async fn pending_transaction_count(&self, address: eth::Address) -> Result<u64, Error> {
        self.web3
            .provider
            .get_transaction_count(address)
            .pending()
            .await
            .map_err(Into::into)
    }

    #[instrument(skip(self), ret(level = Level::DEBUG))]
    pub(super) async fn simulation_gas_price(&self) -> Option<u128> {
        let base_fee = self.current_block().borrow().base_fee;
        // Some nodes don't pick a reasonable default value when you don't
        // specify a gas price and default to 0. Additionally some
        // sneaky tokens have special code paths that detect that case
        // to try to behave differently during simulations
        // than they normally would. To not rely on the node picking a
        // reasonable default value we estimate the current gas price
        // upfront. But because it's extremely rare that tokens behave
        // that way we are fine with falling back to the node specific
        // fallback value instead of failing the whole call.
        Some(self.inner.gas.estimate().await.ok()?.effective(base_fee))
    }

    pub fn web3(&self) -> &Web3 {
        &self.web3
    }
}

impl fmt::Debug for Ethereum {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ethereum")
            .field("web3", &self.web3)
            .field("chain", &self.inner.chain)
            .field("contracts", &self.inner.contracts)
            .field("gas", &"Arc<NativeGasEstimator>")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("method error: {0:?}")]
    ContractRpc(#[from] alloy::contract::Error),
    #[error("alloy rpc error: {0:?}")]
    Rpc(#[from] alloy::transports::RpcError<TransportErrorKind>),
    #[error("gas price estimation error: {0}")]
    GasPrice(boundary::Error),
    #[error("access list estimation error: {0:?}")]
    AccessList(String),
    #[error("trace estimation error")]
    Trace,
    #[error("latest block is unavailable while validating a simulation context")]
    LatestBlockUnavailable,
    #[error(
        "simulation context expired: expected canonical head {expected_number}/{expected_hash:?}, \
         got {actual_number}/{actual_hash:?}"
    )]
    StaleSimulationContext {
        expected_number: u64,
        expected_hash: alloy::primitives::B256,
        actual_number: u64,
        actual_hash: alloy::primitives::B256,
    },
    #[error(
        "simulation target timestamp {target_timestamp} is not after base timestamp \
         {base_timestamp}"
    )]
    InvalidTargetTimestamp {
        base_timestamp: u64,
        target_timestamp: u64,
    },
    #[error("system clock cannot be converted to Unix time: {0}")]
    SystemClock(String),
    #[error(
        "simulation target timestamp {target_timestamp} has expired at current Unix time \
         {current_timestamp}"
    )]
    ExpiredTargetTimestamp {
        current_timestamp: u64,
        target_timestamp: u64,
    },
}

impl Error {
    pub fn into_simulator_error(self, tx: eth::Tx, block: eth::BlockNo) -> simulator::Error {
        let reverted = self.is_revert();
        let err = simulator::SimulatorError::Ethereum(match self {
            Self::Rpc(err) => simulator::ethereum::Error::Rpc(err),
            Self::ContractRpc(err) => simulator::ethereum::Error::ContractRpc(err),
            Self::AccessList(err) => simulator::ethereum::Error::AccessList(err),
            other => simulator::ethereum::Error::Other(anyhow!(other)),
        });
        if reverted {
            simulator::Error::Revert(Box::new(simulator::RevertError { err, tx, block }))
        } else {
            simulator::Error::Other(err)
        }
    }

    /// Returns whether the error indicates that the original transaction
    /// reverted.
    pub fn is_revert(&self) -> bool {
        // This behavior is node dependent
        match self {
            Error::GasPrice(_) => false,
            Error::AccessList(_) => true,
            Error::ContractRpc(_) => true,
            Error::Trace => true,
            Error::LatestBlockUnavailable
            | Error::StaleSimulationContext { .. }
            | Error::InvalidTargetTimestamp { .. }
            | Error::SystemClock(_)
            | Error::ExpiredTargetTimestamp { .. } => false,
            Error::Rpc(err) => {
                let is_revert = err.is_error_resp();
                tracing::trace!(is_revert, ?err, "classified error");
                is_revert
            }
        }
    }
}

impl From<SimulationError> for Error {
    fn from(err: SimulationError) -> Self {
        match err {
            SimulationError::Method(err) => Self::ContractRpc(err),
        }
    }
}

impl From<Ethereum> for simulator::Ethereum {
    fn from(eth: Ethereum) -> Self {
        Self::new(
            eth.web3().clone(),
            eth.chain(),
            configs::simulator::Addresses {
                settlement: Some(*eth.contracts().settlement().address()),
                weth: Some(*eth.contracts().weth().address()),
            },
            eth.gas_estimator(),
            eth.current_block().clone(),
            eth.tx_gas_limit().0,
        )
    }
}

#[cfg(test)]
mod rpc_tests {
    use {
        super::*,
        axum::{Json, Router, extract::State, routing::post},
        ethrpc::block_stream::{BlockInfo, mock_single_block},
        gas_price_estimation::FakeGasPriceEstimator,
        serde_json::{Value, json},
        std::sync::Mutex,
    };

    async fn handle_rpc(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        let tx = &request["params"][0];
        let result = match request["method"].as_str().unwrap() {
            "eth_createAccessList" => json!({"accessList": [], "gasUsed": "0x5208"}),
            "eth_estimateGas" => json!("0x1234"),
            "trace_call" => json!({"output": "0x", "trace": [{
                "type": "call",
                "action": {"callType": "call", "from": tx["from"], "to": tx["to"], "gas": tx["gas"], "input": "0x", "value": "0x0"},
                "result": {"output": "0x", "gasUsed": "0x4567"},
                "subtraces": 0, "traceAddress": []
            }]}),
            name => panic!("unexpected RPC: {name}"),
        };
        let reply = json!({"id": request["id"], "jsonrpc": "2.0", "result": result});
        requests.lock().unwrap().push(request);
        Json(reply)
    }

    #[test]
    fn exact_context_revert_classification() {
        let tx = eth::Tx {
            from: eth::Address::repeat_byte(1),
            to: eth::Address::repeat_byte(2),
            value: 0.into(),
            input: Default::default(),
            access_list: Default::default(),
        };
        let stale = Error::StaleSimulationContext {
            expected_number: 1,
            expected_hash: Default::default(),
            actual_number: 2,
            actual_hash: Default::default(),
        };
        assert!(matches!(
            stale.into_simulator_error(tx.clone(), 1.into()),
            simulator::Error::Other(_)
        ));
        let rejected = Error::Rpc(alloy::transports::RpcError::ErrorResp(
            alloy::rpc::json_rpc::ErrorPayload {
                code: -32000,
                message: "execution reverted".into(),
                data: None,
            },
        ));
        assert!(matches!(
            rejected.into_simulator_error(tx, 1.into()),
            simulator::Error::Revert(_)
        ));
    }

    #[tokio::test]
    async fn simulation_requests_use_one_bounded_latest_call_each() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/", post(handle_rpc))
            .with_state(requests.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let eth = simulator::Ethereum::new(
            boundary::unbuffered_web3(&url),
            Chain::Mainnet,
            Default::default(),
            Arc::new(FakeGasPriceEstimator::default()),
            mock_single_block(BlockInfo::default()),
            eth::U256::from(10_000_000u64),
        );
        let tx = eth::Tx {
            from: eth::Address::repeat_byte(1),
            to: eth::Address::repeat_byte(2),
            value: 0.into(),
            input: Default::default(),
            access_list: Default::default(),
        };
        let simulator = simulator::Simulator::ethereum(eth.clone());
        simulator
            .access_list_for_eth_transfer(tx.from, tx.to)
            .await
            .unwrap();
        assert_eq!(simulator.gas(tx.clone()).await.unwrap(), 0x1234u64.into());
        let tracing = simulator::Simulator::ethereum_trace(eth);
        assert_eq!(tracing.gas(tx).await.unwrap(), 0x4567u64.into());

        let calls = requests.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["method"], "eth_createAccessList");
        assert_eq!(calls[0]["params"][1], "latest");
        assert_eq!(calls[0]["params"][0]["gas"], format!("{:#x}", 2_000_000));
        assert_eq!(calls[1]["method"], "eth_estimateGas");
        assert_eq!(calls[1]["params"][1], "latest");
        assert_eq!(calls[1]["params"][0]["gas"], format!("{:#x}", 10_000_000));
        assert_eq!(calls[2]["method"], "trace_call");
        assert_eq!(calls[2]["params"][1], json!(["trace"]));
        assert_eq!(calls[2]["params"][2], "latest");
        assert_eq!(calls[2]["params"][0]["gas"], format!("{:#x}", 8_000_000));
        server.abort();
    }
}

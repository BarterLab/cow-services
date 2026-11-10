use {
    crate::ethereum::contracts::Contracts,
    alloy_eips::BlockId,
    alloy_primitives::U256,
    alloy_provider::{Provider, ext::TraceApi, network::TransactionBuilder},
    alloy_rpc_types::{TransactionRequest, state::StateOverride, trace::parity::TraceOutput},
    anyhow::{Result, anyhow},
    chain::Chain,
    eth_domain_types::AccessList,
    ethrpc::{Web3, alloy::ProviderLabelingExt, block_stream::CurrentBlockWatcher},
    gas_price_estimation::{Eip1559EstimationExt, GasPriceEstimating},
    std::sync::Arc,
    thiserror::Error,
    tracing::{Level, instrument},
};

pub mod contracts;

#[derive(Clone)]
pub struct Ethereum {
    web3: Web3,
    inner: Arc<Inner>,
}

struct Inner {
    chain: Chain,
    contracts: Contracts,
    gas: Arc<dyn GasPriceEstimating>,
    current_block: CurrentBlockWatcher,
    tx_gas_limit: U256,
}

impl Ethereum {
    pub fn new(
        web3: Web3,
        chain: Chain,
        addresses: configs::simulator::Addresses,
        gas: Arc<dyn GasPriceEstimating>,
        current_block: CurrentBlockWatcher,
        tx_gas_limit: U256,
    ) -> Self {
        Self {
            web3: web3.clone(),
            inner: Arc::new(Inner {
                chain,
                contracts: Contracts::new(web3.clone(), chain, addresses),
                gas,
                current_block,
                tx_gas_limit,
            }),
        }
    }

    #[instrument(skip(self), ret(level = Level::DEBUG))]
    pub(super) async fn simulation_gas_price(&self) -> Option<u128> {
        let base_fee = self.inner.current_block.borrow().base_fee;
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

    pub fn chain(&self) -> Chain {
        self.inner.chain
    }

    pub fn current_block(&self) -> &CurrentBlockWatcher {
        &self.inner.current_block
    }

    pub fn web3(&self) -> &Web3 {
        &self.web3
    }

    /// Clones self and returns an instance that captures metrics extended with
    /// the provided label.
    pub fn with_metric_label(&self, label: String) -> Self {
        Self {
            web3: self.web3.labeled(label),
            ..self.clone()
        }
    }

    /// Cap the transaction's gas limit at the configured per-tx gas limit so a
    /// rogue solution can't force the node to binary-search up to the block gas
    /// limit. `Gas::new` rejects anything above this cap anyway, so no accepted
    /// solution is affected. A cap only lowers, so a tighter ceiling set by the
    /// caller is kept.
    fn cap_gas_limit(&self, tx: TransactionRequest) -> Result<TransactionRequest, Error> {
        let cap = self.inner.tx_gas_limit.try_into().map_err(|err| {
            Error::GasPrice(anyhow!("failed to convert gas_limit to u64: {err:?}"))
        })?;
        let gas_limit = tx.gas.map(|g| g.min(cap)).unwrap_or(cap);
        Ok(tx.with_gas_limit(gas_limit))
    }

    pub async fn create_access_list<T>(&self, tx: T) -> Result<AccessList, Error>
    where
        T: Into<TransactionRequest>,
    {
        let mut tx = self.cap_gas_limit(tx.into())?;
        if self.chain() == Chain::Mainnet {
            tx.gas = tx.gas.map(|gas| gas.min(2_000_000));
        }
        let tx = match self.simulation_gas_price().await {
            Some(gas_price) => tx.with_gas_price(gas_price),
            _ => tx,
        };
        let access_list = self
            .web3
            .provider
            .create_access_list(&tx)
            .latest()
            .await
            .map_err(Error::Rpc)?;
        Ok(access_list
            .ensure_ok()
            .map_err(Error::AccessList)?
            .access_list
            .into())
    }

    pub async fn trace(&self, tx: eth_domain_types::Tx) -> Result<eth_domain_types::Gas, Error> {
        let mut tx = self.cap_gas_limit(tx.into())?;
        if let Some(price) = self.simulation_gas_price().await {
            tx = tx.with_gas_price(price);
        }
        trace_gas(&self.web3, tx).await
    }

    pub async fn estimate_gas<T>(
        &self,
        tx: T,
        overrides: Option<StateOverride>,
        block: BlockId,
    ) -> Result<eth_domain_types::Gas, Error>
    where
        T: Into<TransactionRequest>,
    {
        let tx = self.cap_gas_limit(tx.into())?;
        let tx = match self.simulation_gas_price().await {
            Some(gas_price) => tx.with_gas_price(gas_price),
            _ => tx,
        };

        let estimated_gas = self
            .web3
            .provider
            .estimate_gas(tx)
            .overrides_opt(overrides)
            .block(block)
            .await
            .map_err(Error::Rpc)?
            .into();

        Ok(estimated_gas)
    }
}

/// Return the execution gas of the root call of a bounded latest-block trace.
pub async fn trace_gas(
    web3: &Web3,
    mut tx: TransactionRequest,
) -> Result<eth_domain_types::Gas, Error> {
    tx.gas = Some(tx.gas.unwrap_or(8_000_000).min(8_000_000));
    let trace = web3.provider.trace_call(&tx).latest().await?;
    let root = trace
        .trace
        .into_iter()
        .find(|entry| entry.trace_address.is_empty())
        .ok_or_else(|| Error::Other(anyhow!("missing root transaction trace")))?;
    if let Some(error) = root.error {
        return Err(Error::Other(anyhow!("transaction trace failed: {error}")));
    }
    match root.result {
        Some(TraceOutput::Call(result)) => Ok(result.gas_used.into()),
        _ => Err(Error::Other(anyhow!("missing root call result"))),
    }
}

impl std::fmt::Debug for Ethereum {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
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
    ContractRpc(#[from] alloy_contract::Error),
    #[error("alloy rpc error: {0:?}")]
    Rpc(#[from] alloy_transport::RpcError<alloy_transport::TransportErrorKind>),
    #[error("gas price estimation error: {0}")]
    GasPrice(anyhow::Error),
    #[error("access list estimation error: {0:?}")]
    AccessList(String),
    #[error("other error: {0:?}")]
    Other(anyhow::Error),
}

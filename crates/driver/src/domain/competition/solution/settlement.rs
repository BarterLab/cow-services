use {
    super::{
        Error,
        SimulationContext,
        Solution,
        encoding,
        trade::{self, ClearingPrices},
    },
    crate::{
        domain::{
            self,
            competition::{
                self,
                auction,
                order::{self},
                solution::{self, Interaction, Trade, error},
            },
        },
        infra::{blockchain::Ethereum, solver::ManageNativeToken},
    },
    alloy::{primitives::U256, rpc::types::state::StateOverride},
    eth_domain_types as eth,
    futures::{FutureExt, future::try_join_all},
    simulator::{self, Simulator},
    solvers_dto::solution::StateOverrideBlock,
    std::collections::{BTreeSet, HashMap, HashSet},
    tracing::instrument,
};

/// A solution enriched with everything the driver needs to encode a
/// submission-ready transaction on demand.
///
/// Encoding is two-phase. [`Settlement::new`] runs the invariant checks
/// once (simulation, solver balance, internalization uses only trusted
/// tokens), fetches approvals from the chain, and stores every input
/// that doesn't depend on submission timing. [`Settlement::encode`] is
/// then a synchronous operation that rebuilds the calldata from those
/// cached inputs, splicing in a `DeadlineCheck` pre-interaction bound
/// to a specific submission deadline.
///
/// Publishing a settlement that violates the invariants would result in
/// slashing for the solver (earning reduced rewards). Enforcing them
/// here ensures that the settlement can be broadcast safely with high
/// confidence that it will not be reverted and that it will not result
/// in slashing for the solver.
#[derive(derive_more::Debug, Clone)]
pub struct Settlement {
    /// Auction id appended as bytes to the end of the `settle()` calldata
    pub auction_id: auction::Id,
    /// The gas parameters used by the settlement.
    pub gas: Gas,
    /// Access list required for ETH transfers to smart-contract wallets
    #[debug(ignore)]
    reference_internalized: eth::Tx,
    #[debug(ignore)]
    reference_uninternalized: eth::Tx,
    native: ManageNativeToken,
    #[debug(ignore)]
    solution: Solution,
    /// Native prices used to apply slippage constraints on interactions
    #[debug(ignore)]
    native_prices: auction::Prices,
    /// On-chain approvals needed when encoding with
    /// [`Internalization::Enable`]. Fetched once in [`Settlement::new`] so
    /// [`Settlement::encode`] can stay synchronous and off the RPC path.
    #[debug(ignore)]
    approvals_internalized: Vec<eth::allowance::Approval>,
    /// On-chain approvals needed when encoding with
    /// [`Internalization::Disable`]. Fetched once in [`Settlement::new`].
    #[debug(ignore)]
    approvals_uninternalized: Vec<eth::allowance::Approval>,
}

impl Settlement {
    /// Encode a solution into an onchain settlement.
    #[instrument(name = "create_settlement", skip_all)]
    pub(super) async fn new(
        solution: competition::Solution,
        auction: &competition::Auction,
        auction_id: auction::Id,
        eth: &Ethereum,
        simulator: &Simulator,
        native: ManageNativeToken,
        sender: eth::Address,
    ) -> Result<Self, Error> {
        // For a settlement to be valid, the solution has to respect some rules
        // which would otherwise lead to slashing. Check those rules
        // first.

        // Internalization rule: check that internalized interactions only use
        // trusted tokens.
        let untrusted_tokens = solution
            .interactions
            .iter()
            .filter(|interaction| interaction.internalize())
            .flat_map(|interaction| interaction.inputs())
            .filter(|asset| {
                !auction
                    .tokens()
                    .get(&asset.token)
                    .map(|token| token.trusted)
                    .unwrap_or_default()
            })
            .map(|asset| asset.token)
            .collect::<BTreeSet<_>>();
        if !untrusted_tokens.is_empty() {
            return Err(Error::NonBufferableTokensUsed(untrusted_tokens));
        }

        let native_prices = auction.native_prices();

        let (approvals_enable, approvals_disable) = futures::try_join!(
            solution.approvals(eth, Internalization::Enable),
            solution.approvals(eth, Internalization::Disable),
        )?;
        let approvals_internalized: Vec<_> = approvals_enable.collect();
        let approvals_uninternalized: Vec<_> = approvals_disable.collect();

        let encode_reference = |internalization, approvals: &[eth::allowance::Approval]| {
            let mut tx = encoding::tx(
                auction_id,
                &native_prices,
                &solution,
                eth.contracts(),
                approvals.iter().cloned(),
                internalization,
                native,
                None,
            )?;
            tx.from = sender;
            Ok::<_, Error>(tx)
        };
        let mut internalized = encode_reference(Internalization::Enable, &approvals_internalized)?;
        let mut uninternalized =
            encode_reference(Internalization::Disable, &approvals_uninternalized)?;
        let gas = Self::validate_sender(
            &solution,
            &mut internalized,
            &mut uninternalized,
            eth,
            simulator,
        )
        .await?;
        Ok(Self {
            auction_id,
            gas,
            native,
            solution,
            native_prices,
            approvals_internalized,
            approvals_uninternalized,
            reference_internalized: internalized,
            reference_uninternalized: uninternalized,
        })
    }

    async fn validate_sender(
        solution: &Solution,
        internalized: &mut eth::Tx,
        uninternalized: &mut eth::Tx,
        eth: &Ethereum,
        simulator: &Simulator,
    ) -> Result<Gas, Error> {
        let sender = internalized.from;
        let access_list = try_join_all(
            solution
                .user_trades()
                .map(|trade| partial_access_list_for(trade, sender, solution, eth, simulator)),
        )
        .await?
        .into_iter()
        .flatten()
        .map(|required| required.0)
        .reduce(eth::AccessList::merge)
        .unwrap_or_default();
        internalized.set_access_list(access_list.clone());
        uninternalized.set_access_list(access_list);

        let verify_full = solution.interactions().iter().any(|i| i.internalize());
        if !simulator.access_lists_disabled()
            && (solution.state_overrides.is_some() || solution.state_overrides_block.is_some())
        {
            let extend_access_list = |tx: eth::Tx| async move {
                let partial = tx.access_list.clone();
                let list = eth
                    .create_access_list_with_state_overrides(
                        tx,
                        solution.state_overrides.as_ref(),
                        solution.state_overrides_block,
                    )
                    .await?;
                Ok::<_, Error>(partial.merge(list))
            };
            internalized.set_access_list(extend_access_list(internalized.clone()).await?);
            if verify_full {
                uninternalized.set_access_list(extend_access_list(uninternalized.clone()).await?);
            }
        }

        let gas_used = solution
            .simulate_gas(internalized.clone(), eth, simulator)
            .inspect(|res| {
                tracing::debug!(
                    block = eth.current_block().borrow().number,
                    ?internalized,
                    ?res,
                    "simulated settlement"
                );
            });
        let (gas_used, price, balance) =
            tokio::join!(gas_used, eth.gas_price(), eth.balance(sender));
        let gas = Gas::new(gas_used?, eth.block_gas_limit(), eth.tx_gas_limit())?;
        let max_fee = submission_max_fee_per_gas(
            price?.max_fee_per_gas,
            solution.gas_fee_override().map(|fees| fees.max_fee_per_gas),
        );
        let required = eth::Ether(
            gas.required_balance(max_fee)
                .0
                .saturating_add(internalized.value.0),
        );
        if balance? < required {
            return Err(Error::SolverAccountInsufficientBalance(required));
        }
        if verify_full {
            solution
                .simulate_gas(uninternalized.clone(), eth, simulator)
                .await?;
        }
        Ok(gas)
    }

    pub fn prepared_solution(&self) -> &Solution {
        &self.solution
    }

    /// A state-dependent settlement can only be included in its simulated
    /// block.
    pub fn submission_deadline(&self, requested: eth::BlockNo) -> Option<eth::BlockNo> {
        match self.simulation_context() {
            Some(context) => {
                let target = eth::BlockNo(context.target_block());
                (target <= requested).then_some(target)
            }
            None => Some(requested),
        }
    }

    pub fn transaction(&self, internalization: Internalization) -> &eth::Tx {
        match internalization {
            Internalization::Enable => &self.reference_internalized,
            Internalization::Disable => &self.reference_uninternalized,
        }
    }

    pub async fn for_sender(
        &self,
        sender: eth::Address,
        eth: &Ethereum,
        simulator: &Simulator,
    ) -> Result<Self, Error> {
        let mut updated = self.clone();
        if updated.reference_internalized.from != sender {
            updated.reference_internalized.from = sender;
            updated.reference_uninternalized.from = sender;
            updated.gas = Self::validate_sender(
                &updated.solution,
                &mut updated.reference_internalized,
                &mut updated.reference_uninternalized,
                eth,
                simulator,
            )
            .await?;
        }
        Ok(updated)
    }

    pub fn may_revert(&self) -> bool {
        self.solution.revertable()
    }

    pub fn state_overrides(&self) -> Option<&StateOverride> {
        self.solution.state_overrides.as_ref()
    }

    pub fn state_overrides_block(&self) -> Option<StateOverrideBlock> {
        self.solution.state_overrides_block
    }

    pub fn simulation_context(&self) -> Option<&SimulationContext> {
        self.solution.simulation_context()
    }

    /// Encode the settlement into a submission-ready [`EncodedSettlement`].
    ///
    /// If `deadline` is `Some`, an interaction will be inserted which will
    /// cause a revert when the tx gets mined after the deadline.
    #[instrument(name = "encode_settlement", skip_all)]
    pub fn encode(
        &self,
        eth: &Ethereum,
        deadline: Option<eth::BlockNo>,
    ) -> Result<EncodedSettlement, Error> {
        let deadline = deadline.map(|requested| {
            self.simulation_context().map_or(requested, |context| {
                eth::BlockNo(requested.0.min(context.target_block()))
            })
        });
        let encode_one = |internalization| -> Result<eth::Tx, Error> {
            if deadline.is_none() {
                return Ok(self.transaction(internalization).clone());
            }
            let approvals = match internalization {
                Internalization::Enable => &self.approvals_internalized,
                Internalization::Disable => &self.approvals_uninternalized,
            };

            let mut tx = encoding::tx(
                self.auction_id,
                &self.native_prices,
                &self.solution,
                eth.contracts(),
                approvals.iter().cloned(),
                internalization,
                self.native,
                deadline,
            )?;
            tx.from = self.reference_internalized.from;
            tx.set_access_list(self.transaction(internalization).access_list.clone());
            Ok(tx)
        };

        Ok(EncodedSettlement {
            internalized: encode_one(Internalization::Enable)?,
            uninternalized: encode_one(Internalization::Disable)?,
            gas: self.gas,
            gas_fee_override: self.solution.gas_fee_override(),
            state_overrides: self.solution.state_overrides.clone(),
            state_overrides_block: self.state_overrides_block(),
            simulation_context: self.simulation_context().cloned(),
        })
    }

    /// Score as defined per CIP38. Equal to surplus + protocol fees.
    pub fn score(
        &self,
        prices: &auction::Prices,
        surplus_capturing_jit_order_owners: &HashSet<eth::Address>,
    ) -> Result<eth::Ether, solution::error::Scoring> {
        self.solution
            .scoring(prices, surplus_capturing_jit_order_owners)
    }

    /// The solution encoded in this settlement.
    pub fn solution(&self) -> &super::Id {
        self.solution.id()
    }

    /// Solution's pre interactions
    pub fn pre_interactions(&self) -> &[domain::Interaction] {
        self.solution.pre_interactions()
    }

    /// Solution's interactions
    pub fn interactions(&self) -> &[Interaction] {
        self.solution.interactions()
    }

    /// Solution's post interactions
    pub fn post_interactions(&self) -> &[domain::Interaction] {
        self.solution.post_interactions()
    }

    /// The settled user orders with their in/out amounts.
    pub fn orders(&self) -> HashMap<order::Uid, competition::Amounts> {
        let log_err = |trade: &Trade, err: error::Math, kind: &str| -> eth::TokenAmount {
            // This should never happen, returning 0 is better than panicking,
            // but we should still alert.
            tracing::error!(?trade, prices = ?self.solution.prices, ?err, "could not compute {kind}");
            0.into()
        };
        let mut acc: HashMap<order::Uid, competition::Amounts> = HashMap::new();
        for trade in &self.solution.trades {
            let order = match trade {
                Trade::Fulfillment(_) => {
                    let prices = ClearingPrices {
                        sell: self.solution.prices
                            [&trade.sell().token.as_erc20(self.solution.weth)],
                        buy: self.solution.prices[&trade.buy().token.as_erc20(self.solution.weth)],
                    };
                    competition::Amounts {
                        side: trade.side(),
                        sell: trade.sell(),
                        buy: trade.buy(),
                        executed_sell: trade
                            .sell_amount(&prices)
                            .unwrap_or_else(|err| log_err(trade, err, "executed_sell")),
                        executed_buy: trade
                            .buy_amount(&prices)
                            .unwrap_or_else(|err| log_err(trade, err, "executed_buy")),
                    }
                }
                Trade::Jit(jit) => competition::Amounts {
                    side: trade.side(),
                    sell: trade.sell(),
                    buy: trade.buy(),
                    executed_sell: jit
                        .executed_sell()
                        .unwrap_or_else(|err| log_err(trade, err, "executed_sell")),
                    executed_buy: jit
                        .executed_buy()
                        .unwrap_or_else(|err| log_err(trade, err, "executed_buy")),
                },
            };
            acc.insert(trade.uid(), order);
        }
        acc
    }

    /// The uniform price vector this settlement proposes.
    ///
    /// Deprecated: only emitted on the `/solve` response so that autopilots
    /// running the previous code can deserialise it during a rolling deploy.
    pub fn prices(&self) -> HashMap<eth::TokenAddress, eth::TokenAmount> {
        self.solution
            .clearing_prices()
            .into_iter()
            .map(|(token, amount)| (token, amount.into()))
            .collect()
    }
}

/// Fully-encoded settlement with all additional information the
/// driver needs for the submission logic.
#[derive(derive_more::Debug, Clone)]
pub struct EncodedSettlement {
    /// Transaction that replaced all possible AMM interactions
    /// with interactions trading against settlement contract inventory.
    pub internalized: eth::Tx,
    /// The transaction with all originally provided AMM interactions.
    #[debug(ignore)]
    pub uninternalized: eth::Tx,
    /// Gas parameters (limit used for mempool submission, estimate for
    /// observability).
    pub gas: Gas,
    /// Solver-provided override for the gas fee at submission time.
    pub gas_fee_override: Option<super::GasFeeOverride>,
    #[debug(ignore)]
    pub state_overrides: Option<StateOverride>,
    pub state_overrides_block: Option<StateOverrideBlock>,
    #[debug(ignore)]
    pub simulation_context: Option<SimulationContext>,
}

/// Access lists that are required when the order buys native ETH and the
/// receiver is a smart-contract.
#[derive(Debug, Clone)]
struct RequiredAccessList(eth::AccessList);

/// Returns the partial access list for a single trade, or `None` if the
/// trade does not buy native ETH or its receiver has no on-chain code.
async fn partial_access_list_for(
    trade: &trade::Fulfillment,
    sender: eth::Address,
    solution: &Solution,
    eth: &Ethereum,
    simulator: &Simulator,
) -> Result<Option<RequiredAccessList>, Error> {
    if !trade.order().buys_eth() || !trade.order().pays_to_contract(eth).await? {
        return Ok(None);
    }
    if simulator.access_lists_disabled() {
        return Ok(None);
    }
    let receiver = trade.order().receiver();
    let list = if solution.state_overrides.is_some() || solution.state_overrides_block.is_some() {
        let tx = eth::Tx {
            from: sender,
            to: receiver,
            value: 1.into(),
            input: Default::default(),
            access_list: Default::default(),
        };
        eth.create_access_list_with_state_overrides(
            tx,
            solution.state_overrides.as_ref(),
            solution.state_overrides_block,
        )
        .await?
    } else {
        simulator
            .access_list_for_eth_transfer(sender, receiver)
            .await?
    };
    Ok(Some(RequiredAccessList(list)))
}

/// Should the interactions be internalized?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Internalization {
    /// Internalize interactions which have the `internalize` flag set.
    ///
    /// Since the settlement contract holds balances of multiple tokens, solvers
    /// are in certain cases allowed to "internalize" an AMM interaction, in
    /// order to save on gas.
    ///
    /// <https://docs.cow.fi/off-chain-services/in-depth-solver-specification/output-batch-auction-solutions#using-internal-buffers>
    Enable,
    /// Do not internalize any interactions.
    Disable,
}

/// Gas parameters associated with a settlement.
#[derive(Clone, Copy, Debug)]
pub struct Gas {
    /// The gas estimate, in gas units, for executing a settlement transaction.
    pub estimate: eth::Gas,
    /// The gas limit, in gas units, for a settlement transaction. This is
    /// computed by adding a buffer to the gas estimate to allow for small
    /// variations in the actual gas that gets used.
    pub limit: eth::Gas,
}

impl Gas {
    /// Computes settlement gas parameters given estimates for gas and gas
    /// price.
    pub fn new(
        estimate: eth::Gas,
        block_limit: eth::Gas,
        tx_gas_limit: eth::Gas,
    ) -> Result<Self, solution::Error> {
        // We don't allow for solutions to take up more than half of the block's
        // gas limit. This is to ensure that block producers attempt to
        // include the settlement transaction in the next block as long
        // as it is reasonably priced. If we were to allow for solutions
        // very close to the block gas limit, validators may discard the
        // settlement transaction unless it is paying a very high
        // priority fee. This is because the default block
        // building algorithm picks the highest paying transaction whose gas
        // limit will not exceed the remaining space in the block next
        // and ignore transactions whose gas limit exceed the remaining
        // space (without simulating the actual gas required).
        // Additionally cap by the configured per-tx gas limit. Operators set
        // this per chain (e.g. to EIP-7825's 16,777,215 cap on Mainnet Fusaka)
        // so the mempool can't reject the settlement for exceeding the per-tx
        // ceiling.
        let max_gas = std::cmp::min(eth::Gas(block_limit.0 / eth::U256::from(2)), tx_gas_limit);
        if estimate > max_gas {
            return Err(solution::Error::GasLimitExceeded(estimate, max_gas));
        }

        // Specify a different gas limit than the estimated gas when executing a
        // settlement transaction. This allows the transaction to be resilient
        // to small variations in actual gas usage.
        // Also, some solutions can have significant gas refunds that are
        // refunded at the end of execution, so we want to increase gas
        // limit enough so those solutions don't revert with out of gas
        // error.
        const GAS_LIMIT_FACTOR: f64 = 2.5;
        let estimate_with_buffer = eth::U256::from(f64::from(estimate.0) * GAS_LIMIT_FACTOR).into();

        Ok(Self {
            estimate,
            limit: std::cmp::min(max_gas, estimate_with_buffer),
        })
    }

    /// The balance required to ensure settlement execution with the given gas
    /// parameters.
    pub fn required_balance(&self, max_fee_per_gas: U256) -> eth::Ether {
        self.limit * max_fee_per_gas.into()
    }
}

/// The `max_fee_per_gas` the settlement will be submitted with, used to size
/// the solver's required balance. Mirrors `apply_gas_fee_override`: with an
/// override we check the solver's price; otherwise the driver estimate applies.
fn submission_max_fee_per_gas(
    driver_max_fee_per_gas: u128,
    override_max_fee_per_gas: Option<u128>,
) -> U256 {
    U256::from(override_max_fee_per_gas.unwrap_or(driver_max_fee_per_gas))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gas(value: u64) -> eth::Gas {
        eth::Gas(eth::U256::from(value))
    }

    /// EIP-7825 per-transaction gas cap (2^24 - 1) introduced in Mainnet's
    /// Fusaka hardfork. Used in tests as a representative value for the
    /// configurable `tx_gas_limit` knob on Mainnet.
    const EIP_7825_MAINNET_TX_GAS_CAP: u64 = (1 << 24) - 1;

    #[test]
    fn rejects_solution_above_tx_gas_limit() {
        // Block limit (120M) is high enough that half the block (60M) exceeds
        // the configured per-tx limit (EIP-7825 cap, 16,777,215). The per-tx
        // limit must win.
        let block_limit = gas(120_000_000);
        let tx_gas_limit = gas(EIP_7825_MAINNET_TX_GAS_CAP);
        let estimate = gas(20_000_000);
        let err = Gas::new(estimate, block_limit, tx_gas_limit).unwrap_err();
        match err {
            solution::Error::GasLimitExceeded(used, limit) => {
                assert_eq!(used, estimate);
                assert_eq!(limit, tx_gas_limit);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn accepts_solution_at_tx_gas_limit() {
        let block_limit = gas(120_000_000);
        let tx_gas_limit = gas(EIP_7825_MAINNET_TX_GAS_CAP);
        let result = Gas::new(tx_gas_limit, block_limit, tx_gas_limit).unwrap();
        assert_eq!(result.estimate, tx_gas_limit);
        // The gas buffer would otherwise push the limit above tx_gas_limit; the
        // min(max_gas, ...) clamp must keep it at the configured cap.
        assert_eq!(result.limit, tx_gas_limit);
    }

    #[test]
    fn small_block_limit_still_caps_at_half() {
        // On chains with a low block gas limit, the half-block cap is tighter
        // than the configured per-tx limit and must keep applying.
        let block_limit = gas(10_000_000);
        let tx_gas_limit = gas(EIP_7825_MAINNET_TX_GAS_CAP);
        let estimate = gas(6_000_000);
        let err = Gas::new(estimate, block_limit, tx_gas_limit).unwrap_err();
        match err {
            solution::Error::GasLimitExceeded(_, limit) => assert_eq!(limit, gas(5_000_000)),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn high_tx_gas_limit_lets_half_block_bind() {
        // Non-Fusaka chain: tx_gas_limit configured well above half the block,
        // so the half-block cap is the binding limit.
        let block_limit = gas(120_000_000);
        let tx_gas_limit = gas(100_000_000);
        let estimate = gas(70_000_000);
        let err = Gas::new(estimate, block_limit, tx_gas_limit).unwrap_err();
        match err {
            solution::Error::GasLimitExceeded(_, limit) => assert_eq!(limit, gas(60_000_000)),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn submission_fee_prefers_override_over_driver_estimate() {
        // No override: the current estimated fee.
        assert_eq!(submission_max_fee_per_gas(100, None), U256::from(100));
        // Override above the estimate: use the override.
        assert_eq!(submission_max_fee_per_gas(100, Some(500)), U256::from(500));
        // Override below the estimate: still the override.
        assert_eq!(submission_max_fee_per_gas(100, Some(50)), U256::from(50));
    }
}

/// Wrapper around the gas estimation library.
/// Also allows to add additional tip to the gas price. This is used to
/// increase the chance of a transaction being included in a block, in case
/// private submission networks are used.
use {
    super::Error,
    crate::{
        domain::eth,
        infra::{config::file::GasEstimatorType, mempool},
    },
    alloy::{
        eips::{BlockNumberOrTag, eip1559::Eip1559Estimation},
        providers::Provider,
    },
    anyhow::anyhow,
    ethrpc::Web3,
    shared::gas_price_estimation::{
        GasPriceEstimating,
        configurable_alloy::{ConfigurableGasPriceEstimator, EstimatorConfig},
        eth_node::NodeGasPriceEstimator,
    },
    std::sync::Arc,
};

type MaxAdditionalTip = eth::U256;
type AdditionalTipPercentage = f64;
type AdditionalTip = (MaxAdditionalTip, AdditionalTipPercentage);

pub struct GasPriceEstimator {
    provider: ethrpc::AlloyProvider,
    gas: Arc<dyn GasPriceEstimating>,
    additional_tip: AdditionalTip,
    max_fee_per_gas: eth::U256,
    min_priority_fee: eth::U256,
}

impl GasPriceEstimator {
    pub async fn new(
        web3: &Web3,
        gas_estimator_type: &GasEstimatorType,
        mempools: &[mempool::Config],
    ) -> Result<Self, Error> {
        let gas: Arc<dyn GasPriceEstimating> = match gas_estimator_type {
            GasEstimatorType::Web3 => Arc::new(NodeGasPriceEstimator::new(web3.provider.clone())),
            GasEstimatorType::Alloy {
                past_blocks,
                reward_percentile,
            } => Arc::new(ConfigurableGasPriceEstimator::new(
                web3.provider.clone(),
                EstimatorConfig {
                    past_blocks: *past_blocks,
                    reward_percentile: *reward_percentile,
                },
            )),
        };
        // TODO: simplify logic by moving gas price adjustments out of the individual
        // mempool configs
        let additional_tip = mempools
            .iter()
            .map(|mempool| {
                (
                    mempool.max_additional_tip,
                    mempool.additional_tip_percentage,
                )
            })
            .next()
            .unwrap_or((eth::U256::ZERO, 0.));
        // Use the lowest max_fee_per_gas of all mempools as the max_fee_per_gas
        let max_fee_per_gas = mempools
            .iter()
            .map(|mempool| mempool.gas_price_cap)
            .min()
            .expect("at least one mempool");

        // Use the highest min_priority_fee of all mempools as the min_priority_fee
        let min_priority_fee = mempools
            .iter()
            .map(|mempool| mempool.min_priority_fee)
            .max()
            .expect("at least one mempool");
        Ok(Self {
            provider: web3.provider.clone(),
            gas,
            additional_tip,
            max_fee_per_gas,
            min_priority_fee,
        })
    }

    /// Estimates the gas price for a transaction.
    /// If additional tip is configured, it will be added to the gas price. This
    /// is to increase the chance of a transaction being included in a block, in
    /// case private submission networks are used.
    pub async fn estimate(&self) -> Result<Eip1559Estimation, Error> {
        let estimate = self.gas.estimate().await.map_err(Error::GasPrice)?;

        let mut max_priority_fee_per_gas = {
            // the driver supports tweaking the tx gas price tip in case the gas
            // price estimator is systematically too low => compute configured tip bump
            let (max_additional_tip, tip_percentage_increase) = self.additional_tip;

            // Calculate additional tip in integer space to avoid precision loss
            // Convert percentage to basis points (multiply by 10000) to maintain precision
            // e.g., tip_percentage_increase = 0.125 (12.5%) becomes 1250
            let overflow_err = || {
                Error::GasPrice(anyhow!(
                    "overflow on multiplication (max_priority_fee_per_gas * tip_percentage_as_bps)"
                ))
            };
            let tip_percentage_as_bps = tip_percentage_increase * 10000.0;
            let calculated_tip = eth::U256::from(estimate.max_priority_fee_per_gas)
                .checked_mul(eth::U256::from(tip_percentage_as_bps))
                .ok_or_else(overflow_err)?
                / eth::U256::from(10000u128);

            let additional_tip = max_additional_tip.min(calculated_tip);

            // make sure we tip at least some configurable minimum amount
            std::cmp::max(
                self.min_priority_fee,
                eth::U256::from(estimate.max_priority_fee_per_gas) + additional_tip,
            )
        };

        // Preserve the estimator's base-fee budget when raising the tip, or
        // maxFeePerGas can clip the effective priority fee paid to the builder.
        let base_fee_budget = estimate
            .max_fee_per_gas
            .checked_sub(estimate.max_priority_fee_per_gas)
            .ok_or_else(|| Error::GasPrice(anyhow!("estimated max fee is below priority fee")))?;
        let suggested_max_fee_per_gas = eth::U256::from(base_fee_budget)
            .checked_add(max_priority_fee_per_gas)
            .ok_or_else(|| Error::GasPrice(anyhow!("overflow when adjusting max fee per gas")))?;
        let suggested_max_fee_per_gas = if suggested_max_fee_per_gas > self.max_fee_per_gas {
            // Pay the inclusion block's base fee first. The latest mined
            // block's fee can be higher or lower and must not decide admission.
            let history = self.provider
                .get_fee_history(1, BlockNumberOrTag::Latest, &[])
                .await
                .map_err(|err| Error::GasPrice(err.into()))?;
            let [_, base_fee] = history.base_fee_per_gas.as_slice() else {
                return Err(Error::GasPrice(anyhow!("missing next-block base fee in fee history")));
            };
            let base_fee = *base_fee;
            let available_tip = self.max_fee_per_gas.checked_sub(eth::U256::from(base_fee))
                .ok_or_else(|| {
                    tracing::error!(
                        base_fee,
                        gas_price_cap = %self.max_fee_per_gas,
                        "gas price cap below base fee; cannot submit transaction",
                    );
                    Error::GasPrice(anyhow!(
                        "gas price cap {} is below next-block base fee {}",
                        self.max_fee_per_gas, base_fee,
                    ))
                })?;
            if max_priority_fee_per_gas > available_tip {
                tracing::error!(
                    requested_priority_fee = %max_priority_fee_per_gas,
                    effective_priority_fee = %available_tip,
                    base_fee,
                    gas_price_cap = %self.max_fee_per_gas,
                    "reducing priority fee to fit gas price cap; continuing with capped tip",
                );
                max_priority_fee_per_gas = available_tip;
            }
            tracing::info!(
                requested_max_fee = %suggested_max_fee_per_gas,
                capped_max_fee = %self.max_fee_per_gas,
                base_fee,
                priority_fee = %max_priority_fee_per_gas,
                "reducing base-fee reserve to respect gas price cap",
            );
            self.max_fee_per_gas
        } else {
            suggested_max_fee_per_gas
        };

        Ok(Eip1559Estimation {
            max_fee_per_gas: u128::try_from(suggested_max_fee_per_gas)
                .map_err(|err| Error::GasPrice(err.into()))?,
            max_priority_fee_per_gas: u128::try_from(max_priority_fee_per_gas)
                .map_err(|err| Error::GasPrice(err.into()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{providers::mock::Asserter, rpc::types::FeeHistory},
        shared::gas_price_estimation::FakeGasPriceEstimator,
    };

    fn capped_estimator(cap: u128, history: Result<Vec<u128>, &str>) -> GasPriceEstimator {
        let asserter = Asserter::new();
        match history {
            Ok(fees) => asserter.push_success(&FeeHistory {
                oldest_block: 26_000_000,
                base_fee_per_gas: fees,
                gas_used_ratio: vec![0.5],
                ..Default::default()
            }),
            Err(err) => asserter.push_failure_msg(err.to_owned()),
        }
        GasPriceEstimator {
            provider: Web3::with_asserter(asserter).provider,
            gas: Arc::new(FakeGasPriceEstimator::new(Eip1559Estimation {
                max_fee_per_gas: 210_000_000,
                max_priority_fee_per_gas: 10_000_000,
            })),
            additional_tip: (eth::U256::ZERO, 0.0),
            max_fee_per_gas: eth::U256::from(cap),
            min_priority_fee: eth::U256::from(200_000_000u128),
        }
    }

    #[tokio::test]
    async fn preserves_base_fee_budget_when_adjusting_tip() {
        // (estimated max fee, estimated tip, min tip, additional tip cap,
        // percentage increase, expected tip, expected max fee), all fees in wei.
        let cases: [(u128, u128, u128, u128, f64, u128, u128); 5] = [
            (210_000_000, 10_000_000, 200_000_000, 120_000_000_000, 0.5, 200_000_000, 400_000_000),
            (210_000_000, 10_000_000, 0, 120_000_000_000, 0.5, 15_000_000, 215_000_000),
            (210_000_000, 10_000_000, 0, 2_000_000, 0.5, 12_000_000, 212_000_000),
            (210_000_000, 10_000_000, 0, 0, 0.0, 10_000_000, 210_000_000),
            (200_000_000, 0, 200_000_000, 120_000_000_000, 0.5, 200_000_000, 400_000_000),
        ];
        for (max_fee, tip, min_tip, additional_cap, percentage, expected_tip, expected_max) in cases {
            let estimator = GasPriceEstimator {
                provider: ethrpc::mock::web3().provider,
                gas: Arc::new(FakeGasPriceEstimator::new(Eip1559Estimation {
                    max_fee_per_gas: max_fee,
                    max_priority_fee_per_gas: tip,
                })),
                additional_tip: (eth::U256::from(additional_cap), percentage),
                max_fee_per_gas: eth::U256::from(1_000_000_000_000u128),
                min_priority_fee: eth::U256::from(min_tip),
            };
            let adjusted = estimator.estimate().await.unwrap();
            assert_eq!(adjusted.max_priority_fee_per_gas, expected_tip);
            assert_eq!(adjusted.max_fee_per_gas, expected_max);
            // The configured tip must remain payable throughout the original
            // base-fee budget, including the measured production base fee.
            for base_fee in [86_193_531, max_fee - tip] {
                let effective_tip = adjusted.max_priority_fee_per_gas
                    .min(adjusted.max_fee_per_gas.saturating_sub(base_fee));
                assert_eq!(effective_tip, expected_tip);
            }
        }
    }

    #[tokio::test]
    async fn caps_fees_and_keeps_full_base_fee() {
        for (cap, expected_max, expected_tip) in [
            (500_000_000u128, 400_000_000, 200_000_000),
            (400_000_000, 400_000_000, 200_000_000),
            (399_999_999, 399_999_999, 200_000_000),
            (300_000_000, 300_000_000, 200_000_000),
            (299_999_999, 299_999_999, 199_999_999),
            (150_000_000, 150_000_000, 50_000_000),
            (100_000_000, 100_000_000, 0),
        ] {
            let estimator = capped_estimator(cap, Ok(vec![100_000_000, 100_000_000]));
            let adjusted = estimator.estimate().await.unwrap();
            assert_eq!(adjusted.max_fee_per_gas, expected_max);
            assert_eq!(adjusted.max_priority_fee_per_gas, expected_tip);
            assert_eq!(adjusted.max_priority_fee_per_gas.min(
                adjusted.max_fee_per_gas - 100_000_000,
            ), expected_tip);
        }
    }

    #[tokio::test]
    async fn rejects_unpayable_base_fee_or_unverifiable_cap() {
        for (cap, history) in [
            (99_999_999u128, Ok(vec![100_000_000, 100_000_000])),
            (399_999_999, Ok(vec![])),
            (399_999_999, Ok(vec![100_000_000])),
            (399_999_999, Err("RPC unavailable")),
        ] {
            let estimator = capped_estimator(cap, history);
            assert!(matches!(estimator.estimate().await, Err(Error::GasPrice(_))));
        }
    }

    #[tokio::test]
    async fn cap_uses_inclusion_block_fee_when_base_fee_changes() {
        for (next_base, cap, expected_tip) in [
            (112_500_000u128, 110_000_000u128, None),
            (112_500_000, 150_000_000, Some(37_500_000)),
            (87_500_000, 95_000_000, Some(7_500_000)),
            (87_500_000, 87_500_000, Some(0)),
        ] {
            let estimator = capped_estimator(cap, Ok(vec![100_000_000, next_base]));
            let result = estimator.estimate().await;
            match expected_tip {
                Some(tip) => {
                    let fees = result.unwrap();
                    assert_eq!(fees.max_fee_per_gas, cap);
                    assert_eq!(fees.max_priority_fee_per_gas, tip);
                    assert_eq!(fees.max_fee_per_gas - next_base, tip);
                }
                None => assert!(matches!(result, Err(Error::GasPrice(_)))),
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_or_unrepresentable_fee_estimates() {
        for (max_fee, tip, min_tip) in [
            (9u128, 10u128, eth::U256::ZERO),
            (u128::MAX, 1, eth::U256::from(2u128)),
            (100, 1, eth::U256::MAX),
        ] {
            let estimator = GasPriceEstimator {
                provider: ethrpc::mock::web3().provider,
                gas: Arc::new(FakeGasPriceEstimator::new(Eip1559Estimation {
                    max_fee_per_gas: max_fee,
                    max_priority_fee_per_gas: tip,
                })),
                additional_tip: (eth::U256::ZERO, 0.0),
                max_fee_per_gas: eth::U256::MAX,
                min_priority_fee: min_tip,
            };
            assert!(matches!(estimator.estimate().await, Err(Error::GasPrice(_))));
        }
    }
}

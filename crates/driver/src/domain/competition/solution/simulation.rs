use alloy::{
    eips::BlockId,
    primitives::{B256, U256},
    rpc::types::{BlockOverrides, state::StateOverride},
};
use std::sync::Arc;

/// Exact and immutable context in which a state-dependent solution was verified.
///
/// This value is deliberately not part of the solver DTO. It can only be attached
/// to a domain solution by trusted in-process code after the solver response was
/// validated.
#[derive(Clone)]
pub struct SimulationContext {
    base_number: u64,
    base_hash: B256,
    target_block: u64,
    target_timestamp: u64,
    state_overrides: Arc<StateOverride>,
}

impl SimulationContext {
    pub fn new(
        base_number: u64,
        base_hash: B256,
        target_block: u64,
        target_timestamp: u64,
        state_overrides: StateOverride,
    ) -> Result<Self, Error> {
        if base_hash == B256::ZERO {
            return Err(Error::ZeroBaseBlockHash);
        }
        if target_timestamp == 0 {
            return Err(Error::ZeroTargetTimestamp);
        }
        if state_overrides.is_empty() {
            return Err(Error::EmptyStateOverrides);
        }

        let expected = base_number
            .checked_add(1)
            .ok_or(Error::BaseBlockOverflow)?;
        if target_block != expected {
            return Err(Error::TargetBlockMismatch {
                expected,
                actual: target_block,
            });
        }

        Ok(Self {
            base_number,
            base_hash,
            target_block,
            target_timestamp,
            state_overrides: Arc::new(state_overrides),
        })
    }

    pub const fn base_number(&self) -> u64 {
        self.base_number
    }

    pub const fn base_hash(&self) -> B256 {
        self.base_hash
    }

    pub const fn target_block(&self) -> u64 {
        self.target_block
    }

    pub const fn target_timestamp(&self) -> u64 {
        self.target_timestamp
    }

    /// Whether two exact contexts consume any of the same overridden state.
    /// Disjoint external-state venues can therefore coexist in one auction,
    /// while two settlements competing for the same venue cannot both win.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.state_overrides
            .keys()
            .any(|address| other.state_overrides.contains_key(address))
    }

    pub(crate) fn state_overrides(&self) -> &StateOverride {
        self.state_overrides.as_ref()
    }

    pub(crate) fn base_block_id(&self) -> BlockId {
        BlockId::hash_canonical(self.base_hash)
    }

    pub(crate) fn block_overrides(&self) -> BlockOverrides {
        BlockOverrides::default()
            .with_number(U256::from(self.target_block))
            .with_time(self.target_timestamp)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("simulation state overrides are empty")]
    EmptyStateOverrides,
    #[error("simulation base block hash is zero")]
    ZeroBaseBlockHash,
    #[error("simulation target timestamp is zero")]
    ZeroTargetTimestamp,
    #[error("simulation base block number cannot be incremented")]
    BaseBlockOverflow,
    #[error("simulation target block mismatch: expected {expected}, got {actual}")]
    TargetBlockMismatch { expected: u64, actual: u64 },
    #[error("solution already has a simulation context")]
    AlreadySet,
    #[error("solution has legacy state overrides")]
    LegacyOverridesPresent,
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::Address, rpc::types::state::AccountOverride};

    use super::*;

    #[test]
    fn accepts_exact_context_and_serializes_rpc_quantities() {
        let context = context(
            Address::repeat_byte(0x33),
            0x123_456,
            B256::repeat_byte(0x11),
            0x123_457,
            0x123_459,
        )
        .unwrap();

        assert_eq!(context.base_number(), 0x123_456);
        assert_eq!(context.base_hash(), B256::repeat_byte(0x11));
        assert_eq!(context.target_block(), 0x123_457);
        assert_eq!(context.target_timestamp(), 0x123_459);

        let encoded = serde_json::to_value(context.block_overrides()).unwrap();
        assert_eq!(encoded["number"], "0x123457");
        assert_eq!(encoded["time"], "0x123459");
    }

    #[test]
    fn rejects_wrong_target_block() {
        let Err(error) = context(
            Address::repeat_byte(0x33),
            10,
            B256::repeat_byte(0x11),
            12,
            30,
        ) else {
            panic!("non-next simulation target must be rejected");
        };

        assert!(matches!(
            error,
            Error::TargetBlockMismatch {
                expected: 11,
                actual: 12,
            }
        ));

        let Err(error) = context(
            Address::repeat_byte(0x33),
            u64::MAX,
            B256::repeat_byte(0x11),
            0,
            30,
        ) else {
            panic!("overflowing simulation base must be rejected");
        };
        assert!(matches!(error, Error::BaseBlockOverflow));
    }

    #[test]
    fn rejects_empty_state_and_zero_required_values() {
        assert!(matches!(
            SimulationContext::new(
                10,
                B256::repeat_byte(0x11),
                11,
                30,
                StateOverride::default(),
            ),
            Err(Error::EmptyStateOverrides),
        ));
        assert!(matches!(
            context(
                Address::repeat_byte(0x33),
                10,
                B256::ZERO,
                11,
                30,
            ),
            Err(Error::ZeroBaseBlockHash),
        ));
        assert!(matches!(
            context(
                Address::repeat_byte(0x33),
                10,
                B256::repeat_byte(0x11),
                11,
                0,
            ),
            Err(Error::ZeroTargetTimestamp),
        ));
    }

    #[test]
    fn detects_shared_external_state_without_conflating_disjoint_venues() {
        let first = context_with_address(Address::repeat_byte(0x33));
        let same = context_with_address(Address::repeat_byte(0x33));
        let other = context_with_address(Address::repeat_byte(0x44));

        assert!(first.conflicts_with(&same));
        assert!(!first.conflicts_with(&other));
    }

    fn context_with_address(address: Address) -> SimulationContext {
        context(address, 10, B256::repeat_byte(0x11), 11, 30).unwrap()
    }

    fn context(
        address: Address,
        base_number: u64,
        base_hash: B256,
        target_block: u64,
        target_timestamp: u64,
    ) -> Result<SimulationContext, Error> {
        let mut state = StateOverride::default();
        state.insert(address, AccountOverride::default());

        SimulationContext::new(
            base_number,
            base_hash,
            target_block,
            target_timestamp,
            state,
        )
    }
}

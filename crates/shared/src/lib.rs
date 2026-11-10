#[macro_use]
pub mod macros;

pub mod arguments;
pub mod current_block;
#[cfg(feature = "quoter")]
pub mod db_order_conversions;
#[cfg(feature = "quoter")]
pub mod event_storing_helpers;
pub mod external_prices;
pub mod fee;
pub mod http_solver;
pub mod interaction;
pub mod order_creation_simulation;
#[cfg(feature = "quoter")]
pub mod order_quoting;
#[cfg(feature = "quoter")]
pub mod order_validation;
#[cfg(feature = "quoter")]
pub mod quote_storage;
pub use ::signature_validator;

pub mod price_estimation {
    pub use ::price_estimation::*;

    pub mod trade_verifier {
        pub use ::price_estimation::trade_verifier::*;

        pub mod balance_overrides {
            pub use ::balance_overrides::{
                BalanceOverrideRequest,
                StateOverrides as BalanceOverrides,
                StateOverriding as BalanceOverriding,
            };
        }
    }
}

pub mod remaining_amounts;
pub mod retry;
pub mod token_list;
pub mod url;
pub mod web3;

/// anyhow errors are not clonable natively. This is a workaround that creates a
/// new anyhow error based on formatting the error with its inner sources
/// without backtrace.
pub fn clone_anyhow_error(err: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("{:#}", err)
}

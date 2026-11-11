//! This is a library so that it can be used from e2e tests without having to
//! spawn a process.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod domain;
pub mod infra;
mod run;
pub mod util;

#[cfg(test)]
mod tests;

pub mod contracts {
    pub use ::contracts::{self as alloy, *};
}

pub use {
    self::run::{run, start},
    alloy,
    app_data,
    balance_overrides,
    configs,
    ethrpc,
    model,
    observe,
    serde_ext,
    shared,
    simulator,
    solvers_dto,
};

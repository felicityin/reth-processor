#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Client program input data types.
pub mod io;
#[macro_use]
mod utils;
pub mod custom;
pub mod error;
pub mod executor;
pub mod tracking;

mod into_primitives;
pub use into_primitives::{FromInput, IntoInput, IntoPrimitives, ValidateBlockPostExecution};

use alloy_primitives::FixedBytes;
use executor::{EthClientExecutor, DESERIALZE_INPUTS};
use io::EthClientExecutorInput;
use std::sync::Arc;

pub fn verify_block_hash(input: &Vec<u8>) -> FixedBytes<32> {
    println!("cycle-tracker-report-start: {}", DESERIALZE_INPUTS);
    let input = bincode::deserialize::<EthClientExecutorInput>(input).unwrap();
    println!("cycle-tracker-report-end: {}", DESERIALZE_INPUTS);

    // Execute the block.
    let executor = EthClientExecutor::eth(
        Arc::new((&input.genesis).try_into().unwrap()),
        input.custom_beneficiary,
    );
    let block_hash = match executor.execute(input) {
        Ok(header) => header.hash_slow(),
        Err(e) => {
            println!("Failed to execute block: {:?}", e);
            return FixedBytes::default();
        }
    };
    block_hash
}

use std::{collections::BTreeMap, sync::Arc};

use crate::HostError;
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Sealable, B256};
use alloy_provider::{ext::DebugApi, Network, Provider};
use alloy_rlp::Decodable;
use guest_executor::{
    custom::CustomEvmFactory, io::ClientExecutorInput, IntoInput, IntoPrimitives,
    ValidateBlockPostExecution,
};
use primitives::genesis::Genesis;
use reth_chainspec::ChainSpec;
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use reth_primitives_traits::{Block, NodePrimitives, RecoveredBlock};
use reth_stateless::{
    validation::StatelessValidationError, witness_db::WitnessDatabase, StatelessTrie,
};
use reth_trie_common::{HashedPostState, KeccakKeyHasher};
use reth_trie_zkvm::ZkvmTrie;
use revm_primitives::Address;
use rpc_db::RpcDb;

pub type EthHostExecutor = HostExecutor<EthEvmConfig<ChainSpec, CustomEvmFactory>, ChainSpec>;

pub type OpHostExecutor = HostExecutor<OpEvmConfig, OpChainSpec>;

/// An executor that fetches data from a [Provider] to execute blocks in the [ClientExecutor].
#[derive(Debug, Clone)]
pub struct HostExecutor<C: ConfigureEvm, CS> {
    evm_config: C,
    chain_spec: Arc<CS>,
}

impl EthHostExecutor {
    pub fn eth(chain_spec: Arc<ChainSpec>, custom_beneficiary: Option<Address>) -> Self {
        Self {
            evm_config: EthEvmConfig::new_with_evm_factory(
                chain_spec.clone(),
                CustomEvmFactory::new(custom_beneficiary),
            ),
            chain_spec,
        }
    }
}

impl OpHostExecutor {
    pub fn optimism(chain_spec: Arc<OpChainSpec>) -> Self {
        Self { evm_config: OpEvmConfig::optimism(chain_spec.clone()), chain_spec }
    }
}

impl<C: ConfigureEvm, CS> HostExecutor<C, CS> {
    /// Creates a new [HostExecutor].
    pub fn new(evm_config: C, chain_spec: Arc<CS>) -> Self {
        Self { evm_config, chain_spec }
    }

    /// Executes the block with the given block number.
    pub async fn execute<P, N>(
        &self,
        block_number: u64,
        _rpc_db: &RpcDb<P, N>,
        provider: &P,
        witness_provider: &P,
        genesis: Genesis,
        custom_beneficiary: Option<Address>,
        opcode_tracking: bool,
    ) -> Result<ClientExecutorInput<C::Primitives>, HostError>
    where
        C::Primitives: IntoPrimitives<N> + IntoInput + ValidateBlockPostExecution,
        P: Provider<N> + Clone + 'static,
        N: Network,
    {
        let chain_id: u64 = (&genesis).try_into().unwrap();
        tracing::debug!("chain id: {}", chain_id);
        _ = self.chain_spec.clone();

        // Fetch the current block and the previous block from the provider.
        tracing::info!("[{}] fetching the current block and the previous block", block_number);
        let current_block = provider
            .get_block_by_number(block_number.into())
            .full()
            .await?
            .ok_or(HostError::ExpectedBlock(block_number))
            .map(C::Primitives::into_primitive_block)?;

        let previous_block = provider
            .get_block_by_number((block_number - 1).into())
            .full()
            .await?
            .ok_or(HostError::ExpectedBlock(block_number))
            .map(C::Primitives::into_primitive_block)?;

        tracing::info!("[{}] setting up the witness for the block executor", block_number);
        let witness = witness_provider.debug_execution_witness(block_number.into()).await?;

        tracing::info!("[{}] create state trie", block_number);
        let (parent_state, bytecodes) =
            ZkvmTrie::new(&witness, previous_block.header().state_root())?;
        tracing::info!("[{}] create state trie done", block_number);

        let block = current_block
            .clone()
            .try_into_recovered()
            .map_err(|_| HostError::FailedToRecoverSenders)?;

        let mut ancestor_headers: Vec<Header> = witness
            .headers
            .iter()
            .map(|serialized_header| {
                let bytes = serialized_header.as_ref();
                Header::decode(&mut &bytes[..]).map_err(|_| HostError::HeaderDeserializationFailed)
            })
            .collect::<Result<_, _>>()?;
        // Sort the headers by their block number to ensure that they are in
        // ascending order.
        ancestor_headers.sort_by_key(|header| header.number());

        // if std::env::var("DEBUG_HOST").is_ok() && std::env::var("DEBUG_HOST").unwrap() == "1" {
        {
            let current_block = current_block
                .clone()
                .try_into_recovered()
                .map_err(|_| HostError::FailedToRecoverSenders)?;

            // Check that the ancestor headers form a contiguous chain and are not just random
            // headers.
            let ancestor_hashes =
                self.compute_ancestor_hashes(&current_block, &ancestor_headers)?;

            // Get the last ancestor header and retrieve its state root.
            //
            // There should be at least one ancestor header, this is because we need the parent
            // header to retrieve the previous state root.
            // The edge case here would be the genesis block, but we do not create proofs for the
            // genesis block.
            let pre_state_root = match ancestor_headers.last() {
                Some(prev_header) => prev_header.state_root,
                None => return Err(HostError::MissingAncestorHeader),
            };

            // First verify that the pre-state reads are correct
            let (mut trie, bytecode) = ZkvmTrie::new(&witness, pre_state_root)?;

            // Create an in-memory database that will use the reads to validate the block
            let db = WitnessDatabase::new(&trie, bytecode, ancestor_hashes);

            // Execute the block
            let executor = self.evm_config.executor(db);
            let output = executor.execute(&current_block)?;

            // Validate the block post execution.
            tracing::info!("validating the block post execution");
            C::Primitives::validate_block_post_execution(&block, &genesis, &output)?;

            // Compute and check the post state root
            let hashed_state =
                HashedPostState::from_bundle_state::<KeccakKeyHasher>(&output.state.state);
            let state_root = trie.calculate_state_root(hashed_state)?;
            if state_root != current_block.state_root() {
                return Err(HostError::StateRootMismatch(
                    state_root,
                    current_block.header().state_root(),
                ));
            }

            // Return block hash
            let block_hash = current_block.hash_slow();

            tracing::info!(
                "[{}] successfully validate the block, hash: {:?}",
                block_number,
                block_hash
            );
        }

        // Create the client input.
        let client_input = ClientExecutorInput {
            current_block: C::Primitives::into_input_block(current_block),
            ancestor_headers,
            parent_state,
            state_requests: Default::default(),
            bytecodes: bytecodes.into_values().collect(),
            genesis,
            custom_beneficiary,
            opcode_tracking,
        };
        tracing::info!("[{}] successfully generated client input", block_number);

        Ok(client_input)
    }

    /// Verifies the contiguity, number of ancestor headers and extracts their hashes.
    ///
    /// This function is used to prepare the data required for the `BLOCKHASH`
    /// opcode in a stateless execution context.
    ///
    /// It verifies that the provided `ancestor_headers` form a valid, unbroken chain leading back
    /// from    the parent of the `current_block`.
    ///
    /// Note: This function becomes obsolete if EIP-2935 is implemented.
    /// Note: The headers are assumed to be in ascending order.
    ///
    /// If both checks pass, it returns a [`BTreeMap`] mapping the block number of each
    /// ancestor header to its corresponding block hash.
    fn compute_ancestor_hashes(
        &self,
        current_block: &RecoveredBlock<<C::Primitives as NodePrimitives>::Block>,
        ancestor_headers: &[Header],
    ) -> Result<BTreeMap<u64, B256>, StatelessValidationError> {
        let mut ancestor_hashes = BTreeMap::new();

        let mut parent_hash = current_block.header().parent_hash();
        let mut number = current_block.header().number();

        // Next verify that headers supplied are contiguous
        for parent_header in ancestor_headers.iter().rev() {
            ancestor_hashes.insert(parent_header.number, parent_hash);

            // Blocks must be contiguous
            if parent_hash != parent_header.hash_slow() {
                return Err(StatelessValidationError::InvalidAncestorChain);
            }

            // Header number should be contiguous
            if parent_header.number + 1 != number {
                return Err(StatelessValidationError::InvalidAncestorChain);
            }

            parent_hash = parent_header.parent_hash();
            number = parent_header.number();
        }

        Ok(ancestor_hashes)
    }
}

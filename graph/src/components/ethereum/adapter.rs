use ethabi::{Bytes, Error as ABIError, Function, ParamType, Token};
use failure::{Error, SyncFailure};
use futures::Future;
use slog::Logger;
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use tiny_keccak::keccak256;
use web3::types::*;

use super::types::*;
use crate::prelude::*;

/// A collection of attributes that (kind of) uniquely identify an Ethereum blockchain.
pub struct EthereumNetworkIdentifier {
    pub net_version: String,
    pub genesis_block_hash: H256,
}

/// A request for the state of a contract at a specific block hash and address.
pub struct EthereumContractStateRequest {
    pub address: Address,
    pub block_hash: H256,
}

/// An error that can occur when trying to obtain the state of a contract.
pub enum EthereumContractStateError {
    Failed,
}

/// Representation of an Ethereum contract state.
pub struct EthereumContractState {
    pub address: Address,
    pub block_hash: H256,
    pub data: Bytes,
}

#[derive(Clone, Debug)]
pub struct EthereumContractCall {
    pub address: Address,
    pub block_ptr: EthereumBlockPointer,
    pub function: Function,
    pub args: Vec<Token>,
}

#[derive(Fail, Debug)]
pub enum EthereumContractCallError {
    #[fail(display = "ABI error: {}", _0)]
    ABIError(SyncFailure<ABIError>),
    /// `Token` is not of expected `ParamType`
    #[fail(display = "type mismatch, token {:?} is not of kind {:?}", _0, _1)]
    TypeError(Token, ParamType),
    #[fail(display = "call error: {}", _0)]
    Web3Error(web3::Error),
    #[fail(display = "call reverted: {}", _0)]
    Revert(String),
    #[fail(display = "ethereum node took too long to perform call")]
    Timeout,
}

impl From<ABIError> for EthereumContractCallError {
    fn from(e: ABIError) -> Self {
        EthereumContractCallError::ABIError(SyncFailure::new(e))
    }
}

#[derive(Fail, Debug)]
pub enum EthereumAdapterError {
    /// The Ethereum node does not know about this block for some reason, probably because it
    /// disappeared in a chain reorg.
    #[fail(
        display = "Block data unavailable, block was likely uncled (block hash = {:?})",
        _0
    )]
    BlockUnavailable(H256),

    /// An unexpected error occurred.
    #[fail(display = "Ethereum adapter error: {}", _0)]
    Unknown(Error),
}

impl From<Error> for EthereumAdapterError {
    fn from(e: Error) -> Self {
        EthereumAdapterError::Unknown(e)
    }
}

// A struct for holding all Ethereum filters for a subgraph.
// Each filter is in a hashmap keyed by the data source name so they can be applied selectively.
#[derive(Clone, Debug)]
pub struct EthereumFilters {
    log_filters: HashMap<String, EthereumLogFilter>,
    call_filter: HashMap<String, EthereumCallFilter>,
    block_filters: HashMap<String, EthereumBlockFilter>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EthereumLogFilter {
    pub contract_address_and_event_sig_pairs: HashSet<(Option<u64>, Option<Address>, H256)>,
}

impl EthereumLogFilter {
    /// Check if log bloom filter indicates a possible match for this log filter.
    /// Returns `true` to indicate that a matching `Log` _might_ be contained.
    /// Returns `false` to indicate that a matching `Log` _is not_ contained.
    pub fn check_bloom(&self, _bloom: H2048) -> bool {
        // TODO issue #352: implement bloom filter check
        true // not even wrong
    }

    /// Check if this filter matches the specified `Log`.
    pub fn matches(&self, log: &Log, block_num: &u128) -> bool {
        // First topic should be event sig
        match log.topics.first() {
            None => false,
            Some(sig) => self
                .contract_address_and_event_sig_pairs
                .iter()
                .any(|pair| match pair {
                    // The `Log` matches the filter either if the filter contains
                    // a (contract address, event signature) pair that matches the
                    // `Log`...
                    (start_block, Some(addr), s) => addr == &log.address && s == sig && start_block < block_num,

                    // ...or if the filter contains a pair with no contract address
                    // but an event signature that matches the event
                    (start_block, None, s) => s == sig && start_block < block_num,
                }),
        }
    }

    pub fn from_data_sources<'a>(iter: impl IntoIterator<Item = &'a DataSource>) -> Self {
        let logger = Logger::root(::slog::Discard, o!());

        iter.into_iter()
            .map(|data_source| {
                let contract_addr = data_source.source.address;
                data_source
                    .mapping
                    .event_handlers
                    .iter()
                    .map(move |event_handler| {
                        let event_sig = event_handler.topic0();
                        (
                            data_source
                                .source
                                .start_block
                                .as_ref()
                                .and_then(|num| num.parse::<u64>().ok()),
                            contract_addr,
                            event_sig,
                        )
                    })
            })
            .flatten()
            .collect()
    }

    /// Extends this log filter with another one.
    pub fn extend(&mut self, other: EthereumLogFilter) {
        self.contract_address_and_event_sig_pairs
            .extend(other.contract_address_and_event_sig_pairs.iter());
    }

    /// An empty filter is one that never matches.
    pub fn is_empty(&self) -> bool {
        // Destructure to make sure we're checking all fields.
        let EthereumLogFilter {
            contract_address_and_event_sig_pairs,
        } = self;
        contract_address_and_event_sig_pairs.is_empty()
    }

    pub fn only_activated_filters(&mut self, start_block: u64) -> Self {
        let filtered_set: HashSet<(Option<u64>, Option<Address>, H256)> = self
            .contract_address_and_event_sig_pairs
            .clone()
            .into_iter()
            .filter(|(e, k, sig)| match e {
                Some(block_num) => {
                    if block_num >= &start_block {
                        return true;
                    } else {
                        return false;
                    }
                }
                None => return true,
            })
            .collect();
        Self {
            contract_address_and_event_sig_pairs: filtered_set,
        }
    }
}

impl FromIterator<(Option<u64>, Option<Address>, H256)> for EthereumLogFilter {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (Option<u64>, Option<Address>, H256)>,
    {
        EthereumLogFilter {
            contract_address_and_event_sig_pairs: iter.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EthereumCallFilter {
    pub contract_addresses_function_signatures: HashMap<Address, (Option<u64>, HashSet<[u8; 4]>)>,
}

//pub type EthereumCallFilterAdressesFunctionSignatures = HashMap<>

impl EthereumCallFilter {
    pub fn matches(&self, call: &EthereumCall) -> bool {
        // Ensure the call is to a contract the filter expressed an interest in
        if !self
            .contract_addresses_function_signatures
            .contains_key(&call.to)
        {
            return false;
        }
        // If the call is to a contract with no specified functions, keep the call
        if self
            .contract_addresses_function_signatures
            .get(&call.to)
            .unwrap()
            .1
            .is_empty()
        {
            // Allow the ability to match on calls to a contract generally
            // If you want to match on a generic call to contract this limits you
            // from matching with a specific call to a contract
            return true;
        }
        // Ensure the call is to run a function the filter expressed an interest in
        self.contract_addresses_function_signatures
            .get(&call.to)
            .unwrap()
            .1
            .contains(&call.input.0[..4])
    }

    pub fn from_data_sources<'a>(iter: impl IntoIterator<Item = &'a DataSource>) -> Self {
        iter.into_iter()
            .filter_map(|data_source| data_source.source.address.map(|addr| (addr, data_source)))
            .map(|(contract_addr, data_source)| {
                let start_block = data_source
                    .source
                    .start_block
                    .as_ref()
                    .and_then(|b| b.parse::<u64>().ok());
                data_source
                    .mapping
                    .call_handlers
                    .iter()
                    .map(move |call_handler| {
                        let sig = keccak256(call_handler.function.as_bytes());
                        (start_block, contract_addr, [sig[0], sig[1], sig[2], sig[3]])
                    })
            })
            .flatten()
            .collect()
    }

    /// Extends this call filter with another one.
    pub fn extend(&mut self, other: EthereumCallFilter) {
        self.contract_addresses_function_signatures
            .extend(other.contract_addresses_function_signatures.into_iter());
    }

    /// An empty filter is one that never matches.
    pub fn is_empty(&self) -> bool {
        // Destructure to make sure we're checking all fields.
        let EthereumCallFilter {
            contract_addresses_function_signatures,
        } = self;
        contract_addresses_function_signatures.is_empty()
    }
}

impl FromIterator<(Option<u64>, Address, [u8; 4])> for EthereumCallFilter {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (Option<u64>, Address, [u8; 4])>,
    {
        let mut lookup: HashMap<Address, (Option<u64>, HashSet<[u8; 4]>)> = HashMap::new();
        iter.into_iter()
            .for_each(|(start_block, address, function_signature)| {
                if !lookup.contains_key(&address) {
                    lookup.insert(address, (start_block, HashSet::default()));
                }
                lookup.get_mut(&address).map(|set| {
                    set.1.insert(function_signature);
                    set
                });
            });
        EthereumCallFilter {
            contract_addresses_function_signatures: lookup,
        }
    }
}

impl From<EthereumBlockFilter> for EthereumCallFilter {
    fn from(ethereum_block_filter: EthereumBlockFilter) -> Self {
        Self {
            contract_addresses_function_signatures: ethereum_block_filter
                .contract_addresses
                .into_iter()
                .map(|(start_block_opt, address)| (address, (start_block_opt, HashSet::default())))
                .collect::<HashMap<Address, (Option<u64>, HashSet<[u8; 4]>)>>(),
        }
    }
}

pub struct Checkpoints {
    pub blocks: HashSet<Option<u64>>,
}
type StartBlock = u64;

#[derive(Clone, Debug, Default)]
pub struct EthereumBlockFilter {
    pub contract_addresses: HashSet<(Option<u64>, Address)>,
    pub trigger_every_block: bool,
}

pub type EthereumBlockFilterAddresses = HashSet<Address>;

impl EthereumBlockFilter {
    pub fn from_data_sources<'a>(iter: impl IntoIterator<Item = &'a DataSource>) -> Self {
        let logger = Logger::root(::slog::Discard, o!());
        iter.into_iter()
            .filter(|data_source| data_source.source.address.is_some())
            .fold(Self::default(), |mut filter_opt, data_source| {
                let has_block_handler_with_call_filter = data_source
                    .mapping
                    .block_handlers
                    .clone()
                    .into_iter()
                    .any(|block_handler| match block_handler.filter {
                        Some(ref filter) if *filter == BlockHandlerFilter::Call => return true,
                        _ => return false,
                    });

                let has_block_handler_without_filter = data_source
                    .mapping
                    .block_handlers
                    .clone()
                    .into_iter()
                    .any(|block_handler| block_handler.filter.is_none());

                filter_opt.extend(Self {
                    trigger_every_block: has_block_handler_without_filter,
                    contract_addresses: if has_block_handler_with_call_filter {
                        vec![(
                            data_source
                                .source
                                .start_block
                                .as_ref()
                                .and_then(|b| b.parse::<u64>().ok()),
                            data_source.source.address.unwrap().to_owned(),
                        )]
                        .into_iter()
                        .collect()
                    } else {
                        HashSet::default()
                    },
                });
                filter_opt
            })
    }

    pub fn extend(&mut self, other: EthereumBlockFilter) {
        self.trigger_every_block = self.trigger_every_block || other.trigger_every_block;
        self.contract_addresses.extend(other.contract_addresses);
    }
}

/// Common trait for components that watch and manage access to Ethereum.
///
/// Implementations may be implemented against an in-process Ethereum node
/// or a remote node over RPC.
pub trait EthereumAdapter: Send + Sync + 'static {
    /// Ask the Ethereum node for some identifying information about the Ethereum network it is
    /// connected to.
    fn net_identifiers(
        &self,
        logger: &Logger,
    ) -> Box<dyn Future<Item = EthereumNetworkIdentifier, Error = Error> + Send>;

    /// Find the most recent block.
    fn latest_block(
        &self,
        logger: &Logger,
    ) -> Box<dyn Future<Item = Block<Transaction>, Error = EthereumAdapterError> + Send>;

    /// Find a block by its hash.
    fn block_by_hash(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Box<dyn Future<Item = Option<Block<Transaction>>, Error = Error> + Send>;

    /// Load full information for the specified `block` (in particular, transaction receipts).
    fn load_full_block(
        &self,
        logger: &Logger,
        block: Block<Transaction>,
    ) -> Box<dyn Future<Item = EthereumBlock, Error = EthereumAdapterError> + Send>;

    /// Load full information for the specified `block number` (in particular, transaction receipts).
    fn validate_start_block(
        &self,
        logger: &Logger,
        block_number: u64,
        source_address: Option<H160>,
    ) -> Box<dyn Future<Item = (EthereumBlockPointer, bool), Error = EthereumAdapterError> + Send>;

    /// Find the hash for the parent block of the provided block hash
    fn block_parent_hash(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Box<dyn Future<Item = Option<H256>, Error = Error> + Send>;

    /// Find a block by its number.
    ///
    /// Careful: don't use this function without considering race conditions.
    /// Chain reorgs could happen at any time, and could affect the answer received.
    /// Generally, it is only safe to use this function with blocks that have received enough
    /// confirmations to guarantee no further reorgs, **and** where the Ethereum node is aware of
    /// those confirmations.
    /// If the Ethereum node is far behind in processing blocks, even old blocks can be subject to
    /// reorgs.
    fn block_hash_by_block_number(
        &self,
        logger: &Logger,
        block_number: u64,
    ) -> Box<dyn Future<Item = Option<H256>, Error = Error> + Send>;

    /// Check if `block_ptr` refers to a block that is on the main chain, according to the Ethereum
    /// node.
    ///
    /// Careful: don't use this function without considering race conditions.
    /// Chain reorgs could happen at any time, and could affect the answer received.
    /// Generally, it is only safe to use this function with blocks that have received enough
    /// confirmations to guarantee no further reorgs, **and** where the Ethereum node is aware of
    /// those confirmations.
    /// If the Ethereum node is far behind in processing blocks, even old blocks can be subject to
    /// reorgs.
    fn is_on_main_chain(
        &self,
        logger: &Logger,
        block_ptr: EthereumBlockPointer,
    ) -> Box<dyn Future<Item = bool, Error = Error> + Send>;

    fn calls_in_block(
        &self,
        logger: &Logger,
        block_number: u64,
        block_hash: H256,
    ) -> Box<dyn Future<Item = Vec<EthereumCall>, Error = Error> + Send>;

    fn blocks_with_triggers(
        &self,
        logger: &Logger,
        from: u64,
        to: u64,
        log_filter: EthereumLogFilter,
        call_filter: EthereumCallFilter,
        block_filter: EthereumBlockFilter,
    ) -> Box<dyn Future<Item = Vec<EthereumBlockPointer>, Error = Error> + Send>;

    /// Find the first few blocks in the specified range containing at least one transaction with
    /// at least one log entry matching the specified `log_filter`.
    ///
    /// Careful: don't use this function without considering race conditions.
    /// Chain reorgs could happen at any time, and could affect the answer received.
    /// Generally, it is only safe to use this function with blocks that have received enough
    /// confirmations to guarantee no further reorgs, **and** where the Ethereum node is aware of
    /// those confirmations.
    /// If the Ethereum node is far behind in processing blocks, even old blocks can be subject to
    /// reorgs.
    /// It is recommended that `to` be far behind the block number of latest block the Ethereum
    /// node is aware of.
    fn blocks_with_logs(
        &self,
        logger: &Logger,
        from: u64,
        to: u64,
        log_filter: EthereumLogFilter,
    ) -> Box<dyn Future<Item = Vec<EthereumBlockPointer>, Error = Error> + Send>;

    fn blocks_with_calls(
        &self,
        logger: &Logger,
        from: u64,
        to: u64,
        call_filter: EthereumCallFilter,
    ) -> Box<dyn Future<Item = HashSet<EthereumBlockPointer>, Error = Error> + Send>;

    fn blocks(
        &self,
        logger: &Logger,
        from: u64,
        to: u64,
    ) -> Box<dyn Future<Item = Vec<EthereumBlockPointer>, Error = Error> + Send>;

    /// Call the function of a smart contract.
    fn contract_call(
        &self,
        logger: &Logger,
        call: EthereumContractCall,
    ) -> Box<dyn Future<Item = Vec<Token>, Error = EthereumContractCallError> + Send>;
}

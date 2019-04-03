use std::thread;
use std::time::Instant;

use graph::components::ethereum::*;
use graph::components::store::Store;
use graph::data::subgraph::{DataSource, Source};
use graph::ethabi::{LogParam, Param};
use graph::ethabi::RawLog;
use graph::prelude::{
    RuntimeHost as RuntimeHostTrait, RuntimeHostBuilder as RuntimeHostBuilderTrait, *,
};
use graph::util;
use graph::web3::types::{Log, Transaction};
use super::MappingContext;
use crate::module::{ValidModule, WasmiModule, WasmiModuleConfig};

use futures::sync::mpsc::{channel, Sender};
use futures::sync::oneshot;
use tiny_keccak::keccak256;

pub struct RuntimeHostConfig {
    subgraph_id: SubgraphDeploymentId,
    data_source: DataSource,
}

pub struct RuntimeHostBuilder<T, L, S> {
    ethereum_adapter: Arc<T>,
    link_resolver: Arc<L>,
    store: Arc<S>,
}

impl<T, L, S> Clone for RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    fn clone(&self) -> Self {
        RuntimeHostBuilder {
            ethereum_adapter: self.ethereum_adapter.clone(),
            link_resolver: self.link_resolver.clone(),
            store: self.store.clone(),
        }
    }
}

impl<T, L, S> RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    pub fn new(ethereum_adapter: Arc<T>, link_resolver: Arc<L>, store: Arc<S>) -> Self {
        RuntimeHostBuilder {
            ethereum_adapter,
            link_resolver,
            store,
        }
    }
}

impl<T, L, S> RuntimeHostBuilderTrait for RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    type Host = RuntimeHost;

    fn build(
        &self,
        logger: &Logger,
        subgraph_id: SubgraphDeploymentId,
        data_source: DataSource,
    ) -> Result<Self::Host, Error> {
        RuntimeHost::new(
            logger,
            self.ethereum_adapter.clone(),
            self.link_resolver.clone(),
            self.store.clone(),
            RuntimeHostConfig {
                subgraph_id,
                data_source,
            },
        )
    }
}

type MappingResponse = Result<Vec<EntityOperation>, Error>;

#[derive(Debug)]
struct MappingRequest {
    logger: Logger,
    block: Arc<EthereumBlock>,
    trigger: MappingTrigger,
    entity_operations: Vec<EntityOperation>,
    result_sender: oneshot::Sender<MappingResponse>,
}

#[derive(Debug)]
enum MappingTrigger {
    Log {
        transaction: Arc<Transaction>,
        log: Arc<Log>,
        params: Vec<LogParam>,
        handler: MappingEventHandler,
    },
    Call {
        transaction: Arc<Transaction>,
        call: Arc<EthereumCall>,
        inputs: Vec<LogParam>,
        outputs: Vec<LogParam>,
        handler: MappingTransactionHandler,
    },
    Block {
        handler: MappingBlockHandler,
    },
}

#[derive(Debug)]
pub struct RuntimeHost {
    data_source_name: String,
    data_source_contract: Source,
    data_source_contract_abi: MappingABI,
    data_source_event_handlers: Vec<MappingEventHandler>,
    data_source_call_handlers: Vec<MappingTransactionHandler>,
    data_source_block_handler: Option<MappingBlockHandler>,
    mapping_request_sender: Sender<MappingRequest>,
    _guard: oneshot::Sender<()>,
}

impl RuntimeHost {
    pub fn new<T, L, S>(
        logger: &Logger,
        ethereum_adapter: Arc<T>,
        link_resolver: Arc<L>,
        store: Arc<S>,
        config: RuntimeHostConfig,
    ) -> Result<Self, Error>
    where
        T: EthereumAdapter,
        L: LinkResolver,
        S: Store,
    {
        let logger = logger.new(o!(
            "component" => "RuntimeHost",
            "data_source" => config.data_source.name.clone(),
        ));

        // Create channel for canceling the module
        let (cancel_sender, cancel_receiver) = oneshot::channel();

        // Create channel for event handling requests
        let (mapping_request_sender, mapping_request_receiver) = channel(100);

        // wasmi modules are not `Send` therefore they cannot be scheduled by
        // the regular tokio executor, so we create a dedicated thread.
        //
        // This thread can spawn tasks on the runtime by sending them to
        // `task_receiver`.
        let (task_sender, task_receiver) = channel(100);
        tokio::spawn(task_receiver.for_each(tokio::spawn));

        // Start the WASMI module runtime
        let module_logger = logger.clone();
        let data_source_name = config.data_source.name.clone();
        let data_source_contract = config.data_source.source.clone();
        let data_source_event_handlers = config.data_source.mapping.event_handlers.clone();
        let data_source_call_handlers = config.data_source.mapping.transaction_handlers.clone();
        let data_source_block_handler = config.data_source.mapping.block_handler.clone();
        let data_source_contract_abi = config
            .data_source
            .mapping
            .abis
            .iter()
            .find(|abi| abi.name == config.data_source.source.abi)
            .ok_or_else(|| {
                format_err!(
                    "No ABI entry found for the main contract of data source \"{}\": {}",
                    data_source_name,
                    config.data_source.source.abi,
                )
            })?
            .clone();

        // Spawn a dedicated thread for the runtime.
        //
        // In case of failure, this thread may panic or simply terminate,
        // dropping the `mapping_request_receiver` which ultimately causes the
        // subgraph to fail the next time it tries to handle an event.
        let conf = thread::Builder::new().name(format!(
            "{}-{}-{}",
            util::log::MAPPING_THREAD_PREFIX,
            config.subgraph_id,
            data_source_name
        ));
        conf.spawn(move || {
            debug!(module_logger, "Start WASM runtime");
            let wasmi_config = WasmiModuleConfig {
                subgraph_id: config.subgraph_id,
                data_source: config.data_source,
                ethereum_adapter: ethereum_adapter.clone(),
                link_resolver: link_resolver.clone(),
                store: store.clone(),
            };
            let valid_module = ValidModule::new(&module_logger, wasmi_config, task_sender)
                .expect("Failed to validate module");
            // Pass incoming triggers to the WASM module and return entity changes;
            // Stop when cancelled.
            let canceler = cancel_receiver.into_stream();
            mapping_request_receiver
                .select(canceler.map(|_| panic!("WASM module thread cancelled")).map_err(|_| ()))
                .map_err(|()| err_msg("Cancelled"))
                .for_each(move |request: MappingRequest| -> Result<(), Error> {
                    let MappingRequest {
                        logger,
                        block,
                        entity_operations,
                        trigger,
                        result_sender,
                    } = request;
                    let ctx = MappingContext {
                        logger,
                        block,
                        entity_operations,
                    };
                    let module = WasmiModule::from_valid_module_with_ctx(&valid_module, ctx)?;
                    let result = match trigger {
                        MappingTrigger::Log {
                            transaction, log, params, handler,
                        } => {
                            module.handle_ethereum_log(
                                handler.handler.as_str(),
                                transaction,
                                log,
                                params,
                            )
                        }
                        MappingTrigger::Call {
                            transaction, call, inputs, outputs, handler,
                        } => {
                            module.handle_ethereum_call(
                                handler.handler.as_str(),
                                transaction,
                                call,
                                inputs,
                                outputs,
                            )
                        }
                        MappingTrigger::Block { handler } => {
                            module.handle_ethereum_block(
                                handler.handler.as_str(),
                            )
                        }
                    };
                    result_sender
                        .send(result)
                        .map_err(|_| err_msg("WASM module result receiver dropped."))
                })
                .wait()
                .ok();
        }).expect("Spawning WASM runtime thread failed.");
        Ok(RuntimeHost {
            data_source_name,
            data_source_contract,
            data_source_contract_abi,
            data_source_event_handlers,
            data_source_call_handlers,
            data_source_block_handler,
            mapping_request_sender,
            _guard: cancel_sender,
        })
    }

    fn matches_call_address(&self, call: &EthereumCall) -> bool {
        self.data_source_contract.address == call.to
    }

    fn matches_call_function(&self, call: &EthereumCall) -> bool {
        let target_method_id = &call.input.0[..4];
        self.data_source_call_handlers
            .iter()
            .any(|handler| {
                let fhash = keccak256(handler.function.as_bytes());
                let actual_method_id = [fhash[0], fhash[1], fhash[2], fhash[3]];
                target_method_id == actual_method_id
            })
    }

    fn matches_log_address(&self, log: &Log) -> bool {
        self.data_source_contract.address == log.address
    }

    fn matches_log_signature(&self, log: &Log) -> bool {
        if log.topics.is_empty() {
            return false;
        }
        let signature = log.topics[0];

        self.data_source_event_handlers.iter().any(|event_handler| {
            signature == util::ethereum::string_to_h256(event_handler.event.as_str())
        })
    }

    fn handler_for_log(&self, log: &Arc<Log>) -> Result<MappingEventHandler, Error> {
        // Get signature from the log
        if log.topics.is_empty() {
            return Err(format_err!("Ethereum event has no topics"))
        }
        let signature = log.topics[0];
        self.data_source_event_handlers
            .iter()
            .find(|handler| signature == util::ethereum::string_to_h256(handler.event.as_str()))
            .cloned()
            .ok_or_else(|| {
                format_err!(
                    "No event handler found for event in data source \"{}\"",
                    self.data_source_name,
                )
            })
    }

    fn handler_for_call(&self, call: &Arc<EthereumCall>) -> Result<MappingTransactionHandler, Error> {
        // First four bytes of the input for the call are the first four bytes of hash of the function signature
        if call.input.0.len() < 4 {
            return Err(format_err!("Ethereum call has input with less than 4 bytes"))
        }
        let target_method_id = &call.input.0[..4];
        self.data_source_call_handlers
            .iter()
            .find(move |handler| {
                let fhash = keccak256(handler.function.as_bytes());
                let actual_method_id = [fhash[0], fhash[1], fhash[2], fhash[3]];
                target_method_id == actual_method_id
            })
            .cloned()
            .ok_or_else(|| {
                format_err!(
                    "No call handler found for call in data source \"{}\"",
                    self.data_source_name,
                )
            })
    }

    fn handler_for_block(&self) -> Result<MappingBlockHandler, Error> {
        self
            .data_source_block_handler
            .clone()
            .ok_or_else(|| {
                format_err!(
                    "RuntimeHost does not have a block handler in data source \"{}\"",
                    self.data_source_name,
                )
            })
    }
}

impl RuntimeHostTrait for RuntimeHost {
    fn matches_log(&self, log: &Log) -> bool {
        self.matches_log_address(log) && self.matches_log_signature(log)
    }

    fn matches_call(&self, call: &EthereumCall) -> bool {
        self.matches_call_address(call) && self.matches_call_function(call)
    }

    fn matches_block(&self, block: &EthereumBlock, call: &EthereumCall) -> bool {
        self.data_source_block_handler.is_some() && self.matches_call_address(call)
    }

    fn process_call(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        call: Arc<EthereumCall>,
        entity_operations: Vec<EntityOperation>,
    ) -> Box<Future<Item = Vec<EntityOperation>, Error = Error> + Send> {
        // Identify the call handler for this call
        let call_handler = match self.handler_for_call(&call) {
            Ok(handler) => handler,
            Err(e) => return Box::new(future::err(e)),
        };

        // Identify the function ABI in the contract
        let function_abi = match util::ethereum::contract_function_with_signature(
            &self.data_source_contract_abi.contract,
            call_handler.function.as_str(),
        ) {
            Some(function_abi) => function_abi,
            None => {
                return Box::new(future::err(format_err!(
                    "Function with the signature \"{}\" not found in \
                     contract \"{}\" of data source \"{}\"",
                    call_handler.function,
                    self.data_source_contract_abi.name,
                    self.data_source_name
                )));
            }
        };

        // Parse the inputs
        //
        // Take the input for the call, chop off the first 4 bytes, then call `function.decode_output` to
        // get a vector of `Token`s. Match the `Token`s with the `Param`s in `function.inputs` to
        // create a `Vec<LogParam>`.
        let inputs = match function_abi
            .decode_output(&call.input.0[4..])
            .map_err(|err| {
                format_err!(
                    "Generating function inputs for an Ethereum call failed = {}",
                    err,
                )
            })            
            .and_then(|tokens| {
                if tokens.len() != function_abi.inputs.len() {
                    return Err(format_err!(
                        "Number of arguments in call does not match number of inputs in function signature."
                    ))
                }
                let inputs = tokens
                    .into_iter()
                    .enumerate()
                    .map(|(i, token)| LogParam {
                        name: function_abi.inputs[i].name.clone(),
                        value: token,
                    })
                    .collect::<Vec<LogParam>>();
                Ok(inputs)
            }) {
                Ok(params) => params,
                Err(e) => return Box::new(future::err(e))
            };
        
        // Parse the outputs
        //
        // Take the ouput for the call, then call `function.decode_output` to get a vector of `Token`s.
        // Match the `Token`s with the `Param`s in `function.outputs` to create a `Vec<LogParam>`.
        let outputs = match function_abi
            .decode_output(&call.output.0)
            .map_err(|err| {
                format_err!(
                    "Generating function outputs for an Ethereum call failed = {}",
                    err,
                )
            })
            .and_then(|tokens| {
                if tokens.len() != function_abi.outputs.len() {
                    return Err(format_err!(
                        "Number of paramters in the call output does not match number of outputs in the function signature."
                    ))
                }
                let outputs = tokens
                    .into_iter()
                    .enumerate()
                    .map(|(i, token)| LogParam {
                        name: function_abi.outputs[i].name.clone(),
                        value: token,
                    })
                    .collect::<Vec<LogParam>>();
                Ok(outputs)
            }) {
                Ok(outputs) => outputs,
                Err(e) => return Box::new(future::err(e))
            };


        // Execute the call handler and asynchronously wait for the result
        let (result_sender, result_receiver) = oneshot::channel();
        let start_time = Instant::now();
        let eops = self.mapping_request_sender
            .clone()
            .send(MappingRequest {
                logger: logger.clone(),
                block: block.clone(),
                trigger: MappingTrigger::Call {
                    transaction: transaction.clone(),
                    call: call.clone(),
                    inputs,
                    outputs,
                    handler: call_handler.clone(),
                },
                entity_operations,
                result_sender,
            })
            .map_err(move |_| {
                format_err!(
                    "Mapping terminated before passing in Ethereum call",
                )
            })
            .and_then(|_| {
                result_receiver.map_err(move |_| {
                    format_err!(
                        "Mapping terminated before finishing to handle",
                    )
                })
            })
            .and_then(move |result| {
                info!(
                    logger, "Done processing Ethereum call";
                    // Replace this when `as_millis` is stable.
                    "secs" => start_time.elapsed().as_secs(),
                    "ms" => start_time.elapsed().subsec_millis()
                );
                result
            });
        Box::new(eops)
    }

    fn process_block(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        entity_operations: Vec<EntityOperation>,
    ) -> Box<Future<Item = Vec<EntityOperation>, Error = Error> + Send> {
        let block_handler = match self.handler_for_block() {
            Ok(handler) => handler,
            Err(e) => return Box::new(future::err(e)),
        };

        // Execute the call handler and asynchronously wait for the result
        let (result_sender, result_receiver) = oneshot::channel();
        let start_time = Instant::now();
        let eops = self.mapping_request_sender
            .clone()
            .send(MappingRequest {
                logger: logger.clone(),
                block: block.clone(),
                trigger: MappingTrigger::Block {
                    handler: block_handler.clone(),
                },
                entity_operations,
                result_sender,
            })
            .map_err(move |_| {
                format_err!(
                    "Mapping terminated before passing in Ethereum block",
                )
            })
            .and_then(|_| {
                result_receiver.map_err(move |_| {
                    format_err!(
                        "Mapping terminated before finishing to handle",
                    )
                })
            })
            .and_then(move |result| {
                info!(
                    logger, "Done processing Ethereum block";
                    // Replace this when `as_millis` is stable.
                    "secs" => start_time.elapsed().as_secs(),
                    "ms" => start_time.elapsed().subsec_millis()
                );
                result
            });
        Box::new(eops)
    }

    fn process_log(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        log: Arc<Log>,
        entity_operations: Vec<EntityOperation>,
    ) -> Box<Future<Item = Vec<EntityOperation>, Error = Error> + Send> {
        // Identify event handler for this log
        let event_handler = match self.handler_for_log(&log) {
            Ok(handler) => handler,
            Err(e) => return Box::new(future::err(e)),
        };

        // Identify the event ABI in the contract
        let event_abi = match util::ethereum::contract_event_with_signature(
            &self.data_source_contract_abi.contract,
            event_handler.event.as_str(),
        ) {
            Some(event_abi) => event_abi,
            None => {
                return Box::new(future::err(format_err!(
                    "Event with the signature \"{}\" not found in \
                     contract \"{}\" of data source \"{}\"",
                    event_handler.event,
                    self.data_source_contract_abi.name,
                    self.data_source_name
                )));
            }
        };

        // Parse the log into an event
        let params = match event_abi.parse_log(RawLog {
            topics: log.topics.clone(),
            data: log.data.clone().0,
        }) {
            Ok(log) => log.params,
            Err(e) => {
                return Box::new(future::err(format_err!(
                    "Failed to parse parameters of event: {}: {}",
                    event_handler.event,
                    e
                )));
            }
        };

        debug!(
            logger, "Start processing Ethereum event";
            "signature" => &event_handler.event,
            "handler" => &event_handler.handler
        );

        // Call the event handler and asynchronously wait for the result
        let (result_sender, result_receiver) = oneshot::channel();

        let before_event_signature = event_handler.event.clone();
        let event_signature = event_handler.event.clone();
        let start_time = Instant::now();

        let eops = self.mapping_request_sender
            .clone()
            .send(MappingRequest {
                logger: logger.clone(),
                block: block.clone(),
                trigger: MappingTrigger::Log {
                    transaction: transaction.clone(),
                    log: log.clone(),
                    params,
                    handler: event_handler.clone(),
                },
                entity_operations,
                result_sender,
            })
            .map_err(move |_| {
                format_err!(
                    "Mapping terminated before passing in Ethereum event: {}",
                    before_event_signature
                )
            })
            .and_then(|_| {
                result_receiver.map_err(move |_| {
                    format_err!(
                        "Mapping terminated before finishing to handle \
                         Ethereum event: {}",
                        event_signature,
                    )
                })
            })
            .and_then(move |result| {
                info!(
                    logger, "Done processing Ethereum event";
                    "signature" => &event_handler.event,
                    "handler" => &event_handler.handler,
                    // Replace this when `as_millis` is stable.
                    "secs" => start_time.elapsed().as_secs(),
                    "ms" => start_time.elapsed().subsec_millis()
                );
                result
            });
        Box::new(eops)
    }
}

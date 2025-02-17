use itertools::Itertools;

use std::time::Instant;

use zksync_types::{
    api::{
        BlockId, BlockNumber, GetLogsFilter, Transaction, TransactionId, TransactionReceipt,
        TransactionVariant,
    },
    l2::{L2Tx, TransactionType},
    transaction_request::{l2_tx_from_call_req, CallRequest},
    utils::decompose_full_nonce,
    web3::types::{SyncInfo, SyncState},
    AccountTreeId, Bytes, MiniblockNumber, StorageKey, H256, L2_ETH_TOKEN_ADDRESS,
    MAX_GAS_PER_PUBDATA_BYTE, U256,
};
use zksync_utils::u256_to_h256;
use zksync_web3_decl::{
    error::Web3Error,
    types::{Address, Block, Filter, FilterChanges, Log, TypedFilter, U64},
};
#[cfg(feature = "openzeppelin_tests")]
use {
    zksync_eth_signer::EthereumSigner,
    zksync_types::{
        api::TransactionRequest, storage::CONTRACT_DEPLOYER_ADDRESS,
        transaction_request::Eip712Meta, web3::contract::tokens::Tokenizable, Eip712Domain,
        EIP_712_TX_TYPE,
    },
    zksync_utils::bytecode::hash_bytecode,
};

use crate::{
    api_server::{
        execution_sandbox::BlockArgs,
        web3::{backend_jsonrpc::error::internal_error, resolve_block, state::RpcState},
    },
    l1_gas_price::L1GasPriceProvider,
};

pub const EVENT_TOPIC_NUMBER_LIMIT: usize = 4;
pub const PROTOCOL_VERSION: &str = "zks/1";

#[derive(Debug)]
pub struct EthNamespace<G> {
    state: RpcState<G>,
}

impl<G> Clone for EthNamespace<G> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<G: L1GasPriceProvider> EthNamespace<G> {
    pub fn new(state: RpcState<G>) -> Self {
        Self { state }
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_block_number_impl(&self) -> Result<U64, Web3Error> {
        const METHOD_NAME: &str = "get_block_number";

        let start = Instant::now();
        let block_number = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .blocks_web3_dal()
            .get_sealed_miniblock_number()
            .await
            .map(|n| U64::from(n.0))
            .map_err(|err| internal_error(METHOD_NAME, err));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        block_number
    }

    #[tracing::instrument(skip(self, request, block))]
    pub async fn call_impl(
        &self,
        request: CallRequest,
        block: Option<BlockId>,
    ) -> Result<Bytes, Web3Error> {
        let start = Instant::now();

        let block = block.unwrap_or(BlockId::Number(BlockNumber::Pending));
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await;
        let block_args = BlockArgs::new(&mut connection, block)
            .await
            .map_err(|err| internal_error("eth_call", err))?
            .ok_or(Web3Error::NoBlock)?;
        drop(connection);

        let mut request_with_set_nonce = request.clone();
        self.state
            .set_nonce_for_call_request(&mut request_with_set_nonce)
            .await?;

        #[cfg(not(feature = "openzeppelin_tests"))]
        let tx = l2_tx_from_call_req(request, self.state.api_config.max_tx_size)?;
        #[cfg(feature = "openzeppelin_tests")]
        let tx: L2Tx = self
            .convert_evm_like_deploy_requests(tx_req_from_call_req(
                request,
                self.state.api_config.max_tx_size,
            )?)?
            .try_into()?;

        let call_result = self.state.tx_sender.eth_call(block_args, tx).await;
        let mut res_bytes = call_result
            .map_err(|err| Web3Error::SubmitTransactionError(err.to_string(), err.data()))?;

        if cfg!(feature = "openzeppelin_tests")
            && res_bytes.len() >= 100
            && hex::encode(&res_bytes[96..100]).as_str() == "08c379a0"
        {
            res_bytes = res_bytes[96..].to_vec();
        }

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "call");
        Ok(res_bytes.into())
    }

    #[tracing::instrument(skip(self, request, _block))]
    pub async fn estimate_gas_impl(
        &self,
        request: CallRequest,
        _block: Option<BlockNumber>,
    ) -> Result<U256, Web3Error> {
        let start = Instant::now();
        let mut request_with_gas_per_pubdata_overridden = request;

        self.state
            .set_nonce_for_call_request(&mut request_with_gas_per_pubdata_overridden)
            .await?;

        if let Some(ref mut eip712_meta) = request_with_gas_per_pubdata_overridden.eip712_meta {
            if eip712_meta.gas_per_pubdata == U256::zero() {
                eip712_meta.gas_per_pubdata = MAX_GAS_PER_PUBDATA_BYTE.into();
            }
        }

        let is_eip712 = request_with_gas_per_pubdata_overridden
            .eip712_meta
            .is_some();

        #[cfg(not(feature = "openzeppelin_tests"))]
        let mut tx: L2Tx = l2_tx_from_call_req(
            request_with_gas_per_pubdata_overridden,
            self.state.api_config.max_tx_size,
        )?;

        #[cfg(feature = "openzeppelin_tests")]
        let mut tx: L2Tx = self
            .convert_evm_like_deploy_requests(tx_req_from_call_req(
                request_with_gas_per_pubdata_overridden,
                self.state.api_config.max_tx_size,
            )?)?
            .try_into()?;

        // The user may not include the proper transaction type during the estimation of
        // the gas fee. However, it is needed for the bootloader checks to pass properly.
        if is_eip712 {
            tx.common_data.transaction_type = TransactionType::EIP712Transaction;
        }

        // When we're estimating fee, we are trying to deduce values related to fee, so we should
        // not consider provided ones.

        tx.common_data.fee.max_fee_per_gas = self.state.tx_sender.gas_price().into();
        tx.common_data.fee.max_priority_fee_per_gas = tx.common_data.fee.max_fee_per_gas;

        // Modify the l1 gas price with the scale factor
        let scale_factor = self.state.api_config.estimate_gas_scale_factor;
        let acceptable_overestimation =
            self.state.api_config.estimate_gas_acceptable_overestimation;

        let fee = self
            .state
            .tx_sender
            .get_txs_fee_in_wei(tx.into(), scale_factor, acceptable_overestimation)
            .await
            .map_err(|err| Web3Error::SubmitTransactionError(err.to_string(), err.data()))?;

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "estimate_gas");
        Ok(fee.gas_limit)
    }

    #[tracing::instrument(skip(self))]
    pub fn gas_price_impl(&self) -> Result<U256, Web3Error> {
        const METHOD_NAME: &str = "gas_price";

        let start = Instant::now();
        let price = self.state.tx_sender.gas_price();
        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        Ok(price.into())
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_balance_impl(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> Result<U256, Web3Error> {
        const METHOD_NAME: &str = "get_balance";

        let start = Instant::now();
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await;
        let block = block.unwrap_or(BlockId::Number(BlockNumber::Pending));
        let block_number = resolve_block(&mut connection, block, METHOD_NAME).await?;
        let balance = connection
            .storage_web3_dal()
            .standard_token_historical_balance(
                AccountTreeId::new(L2_ETH_TOKEN_ADDRESS),
                AccountTreeId::new(address),
                block_number,
            )
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;
        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        Ok(balance)
    }

    #[tracing::instrument(skip(self, filter))]
    pub async fn get_logs_impl(&self, mut filter: Filter) -> Result<Vec<Log>, Web3Error> {
        let start = Instant::now();

        self.state.resolve_filter_block_hash(&mut filter).await?;
        let (from_block, to_block) = self.state.resolve_filter_block_range(&filter).await?;

        filter.to_block = Some(BlockNumber::Number(to_block.0.into()));
        let changes = self
            .filter_changes(TypedFilter::Events(filter, from_block))
            .await?
            .0;

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "get_logs");
        Ok(match changes {
            FilterChanges::Logs(list) => list,
            _ => unreachable!("Unexpected `FilterChanges` type, expected `Logs`"),
        })
    }

    // #[tracing::instrument(skip(self))]
    pub async fn get_filter_logs_impl(&self, idx: U256) -> Result<FilterChanges, Web3Error> {
        let start = Instant::now();

        // Note: We have to keep this as a separate variable, since otherwise the lock guard would exist
        // for duration of the whole `match` block, and this guard is not `Send`. This would make the whole future
        // not `Send`, since `match` has an `await` point.
        let maybe_filter = self.state.installed_filters.read().await.get(idx).cloned();
        let filter = match maybe_filter {
            Some(TypedFilter::Events(filter, _)) => {
                let from_block = self
                    .state
                    .resolve_filter_block_number(filter.from_block)
                    .await?;
                TypedFilter::Events(filter, from_block)
            }
            _ => return Err(Web3Error::FilterNotFound),
        };

        let logs = self.filter_changes(filter).await?.0;

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "get_filter_logs");
        Ok(logs)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_block_impl(
        &self,
        block: BlockId,
        full_transactions: bool,
    ) -> Result<Option<Block<TransactionVariant>>, Web3Error> {
        let start = Instant::now();
        let method_name = if full_transactions {
            "get_block_with_txs"
        } else {
            "get_block"
        };

        let block = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .blocks_web3_dal()
            .get_block_by_web3_block_id(block, full_transactions, self.state.api_config.l2_chain_id)
            .await
            .map_err(|err| internal_error(method_name, err));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => method_name);
        block
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_block_transaction_count_impl(
        &self,
        block: BlockId,
    ) -> Result<Option<U256>, Web3Error> {
        const METHOD_NAME: &str = "get_block_transaction_count";

        let start = Instant::now();
        let tx_count = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .blocks_web3_dal()
            .get_block_tx_count(block)
            .await
            .map_err(|err| internal_error(METHOD_NAME, err));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        tx_count
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_code_impl(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> Result<Bytes, Web3Error> {
        const METHOD_NAME: &str = "get_code";

        let start = Instant::now();
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await;
        let block = block.unwrap_or(BlockId::Number(BlockNumber::Pending));
        let block_number = resolve_block(&mut connection, block, METHOD_NAME).await?;
        let contract_code = connection
            .storage_web3_dal()
            .get_contract_code_unchecked(address, block_number)
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        Ok(contract_code.unwrap_or_default().into())
    }

    #[tracing::instrument(skip(self))]
    pub fn chain_id_impl(&self) -> U64 {
        self.state.api_config.l2_chain_id.0.into()
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_storage_at_impl(
        &self,
        address: Address,
        idx: U256,
        block: Option<BlockId>,
    ) -> Result<H256, Web3Error> {
        const METHOD_NAME: &str = "get_storage_at";

        let start = Instant::now();
        let block = block.unwrap_or(BlockId::Number(BlockNumber::Pending));
        let storage_key = StorageKey::new(AccountTreeId::new(address), u256_to_h256(idx));
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await;
        let block_number = resolve_block(&mut connection, block, METHOD_NAME).await?;
        let value = connection
            .storage_web3_dal()
            .get_historical_value_unchecked(&storage_key, block_number)
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        Ok(value)
    }

    /// Account nonce.
    #[tracing::instrument(skip(self))]
    pub async fn get_transaction_count_impl(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> Result<U256, Web3Error> {
        let start = Instant::now();
        let block = block.unwrap_or(BlockId::Number(BlockNumber::Pending));

        let method_name = match block {
            BlockId::Number(BlockNumber::Pending) => "get_pending_transaction_count",
            _ => "get_historical_transaction_count",
        };
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await;

        let full_nonce = match block {
            BlockId::Number(BlockNumber::Pending) => connection
                .transactions_web3_dal()
                .next_nonce_by_initiator_account(address)
                .await
                .map_err(|err| internal_error(method_name, err)),
            _ => {
                let block_number = resolve_block(&mut connection, block, method_name).await?;
                connection
                    .storage_web3_dal()
                    .get_address_historical_nonce(address, block_number)
                    .await
                    .map_err(|err| internal_error(method_name, err))
            }
        };

        let account_nonce = full_nonce.map(|nonce| decompose_full_nonce(nonce).0);

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => method_name);
        account_nonce
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_transaction_impl(
        &self,
        id: TransactionId,
    ) -> Result<Option<Transaction>, Web3Error> {
        const METHOD_NAME: &str = "get_transaction";

        let start = Instant::now();
        let mut transaction = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .transactions_web3_dal()
            .get_transaction(id, self.state.api_config.l2_chain_id)
            .await
            .map_err(|err| internal_error(METHOD_NAME, err));

        if let Some(proxy) = &self.state.tx_sender.0.proxy {
            // We're running an external node - check the proxy cache in
            // case the transaction was proxied but not yet synced back to us
            if let Ok(Some(tx)) = &transaction {
                // If the transaction is already in the db, remove it from cache
                proxy.forget_tx(tx.hash).await
            } else {
                if let TransactionId::Hash(hash) = id {
                    // If the transaction is not in the db, check the cache
                    if let Some(tx) = proxy.find_tx(hash).await {
                        transaction = Ok(Some(tx.into()));
                    }
                }
                if !matches!(transaction, Ok(Some(_))) {
                    // If the transaction is not in the db or cache, query main node
                    transaction = proxy
                        .request_tx(id)
                        .await
                        .map_err(|err| internal_error(METHOD_NAME, err));
                }
            }
        }

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        transaction
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_transaction_receipt_impl(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionReceipt>, Web3Error> {
        const METHOD_NAME: &str = "get_transaction_receipt";

        let start = Instant::now();
        let mut receipt = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .transactions_web3_dal()
            .get_transaction_receipt(hash)
            .await
            .map_err(|err| internal_error(METHOD_NAME, err));

        if let Some(proxy) = &self.state.tx_sender.0.proxy {
            // We're running an external node
            if matches!(receipt, Ok(None)) {
                // If the transaction is not in the db, query main node.
                // Because it might be the case that it got rejected in state keeper
                // and won't be synced back to us, but we still want to return a receipt.
                // We want to only forward these kinds of receipts because otherwise
                // clients will assume that the transaction they got the receipt for
                // was already processed on the EN (when it was not),
                // and will think that the state has already been updated on the EN (when it was not).
                if let Ok(Some(main_node_receipt)) = proxy
                    .request_tx_receipt(hash)
                    .await
                    .map_err(|err| internal_error(METHOD_NAME, err))
                {
                    if main_node_receipt.status == Some(0.into())
                        && main_node_receipt.block_number.is_none()
                    {
                        // Transaction was rejected in state-keeper.
                        receipt = Ok(Some(main_node_receipt));
                    }
                }
            }
        }

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        receipt
    }

    #[tracing::instrument(skip(self))]
    pub async fn new_block_filter_impl(&self) -> Result<U256, Web3Error> {
        const METHOD_NAME: &str = "new_block_filter";

        let start = Instant::now();
        let last_block_number = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .blocks_web3_dal()
            .get_sealed_miniblock_number()
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;

        let idx = self
            .state
            .installed_filters
            .write()
            .await
            .add(TypedFilter::Blocks(last_block_number));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => METHOD_NAME);
        Ok(idx)
    }

    #[tracing::instrument(skip(self, filter))]
    pub async fn new_filter_impl(&self, mut filter: Filter) -> Result<U256, Web3Error> {
        let start = Instant::now();

        if let Some(topics) = filter.topics.as_ref() {
            if topics.len() > EVENT_TOPIC_NUMBER_LIMIT {
                return Err(Web3Error::TooManyTopics);
            }
        }
        self.state.resolve_filter_block_hash(&mut filter).await?;
        let from_block = self.state.get_filter_from_block(&filter).await?;
        let idx = self
            .state
            .installed_filters
            .write()
            .await
            .add(TypedFilter::Events(filter, from_block));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "new_filter");
        Ok(idx)
    }

    #[tracing::instrument(skip(self))]
    pub async fn new_pending_transaction_filter_impl(&self) -> U256 {
        let start = Instant::now();

        let idx = self
            .state
            .installed_filters
            .write()
            .await
            .add(TypedFilter::PendingTransactions(
                chrono::Utc::now().naive_utc(),
            ));

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "new_pending_transaction_filter");
        idx
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_filter_changes_impl(&self, idx: U256) -> Result<FilterChanges, Web3Error> {
        let start = Instant::now();

        let filter = self
            .state
            .installed_filters
            .read()
            .await
            .get(idx)
            .cloned()
            .ok_or(Web3Error::FilterNotFound)?;

        let result = match self.filter_changes(filter).await {
            Ok((changes, updated_filter)) => {
                self.state
                    .installed_filters
                    .write()
                    .await
                    .update(idx, updated_filter);
                Ok(changes)
            }
            Err(Web3Error::LogsLimitExceeded(_, _, _)) => {
                // The filter was not being polled for a long time, so we remove it.
                self.state.installed_filters.write().await.remove(idx);
                Err(Web3Error::FilterNotFound)
            }
            Err(err) => Err(err),
        };

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "get_filter_changes");
        result
    }

    #[tracing::instrument(skip(self))]
    pub async fn uninstall_filter_impl(&self, idx: U256) -> bool {
        let start = Instant::now();

        let removed = self.state.installed_filters.write().await.remove(idx);

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "uninstall_filter");
        removed
    }

    #[tracing::instrument(skip(self))]
    pub fn protocol_version(&self) -> String {
        PROTOCOL_VERSION.to_string()
    }

    #[tracing::instrument(skip(self, tx_bytes))]
    pub async fn send_raw_transaction_impl(&self, tx_bytes: Bytes) -> Result<H256, Web3Error> {
        let start = Instant::now();
        let (mut tx, hash) = self.state.parse_transaction_bytes(&tx_bytes.0)?;
        tx.set_input(tx_bytes.0, hash);

        let submit_result = self.state.tx_sender.submit_tx(tx).await;
        let submit_result = submit_result.map(|_| hash).map_err(|err| {
            vlog::debug!("Send raw transaction error: {err}");
            metrics::counter!(
                "api.submit_tx_error",
                1,
                "reason" => err.grafana_error_code()
            );
            Web3Error::SubmitTransactionError(err.to_string(), err.data())
        });

        metrics::histogram!("api.web3.call", start.elapsed(), "method" => "send_raw_transaction");
        submit_result
    }

    #[tracing::instrument(skip(self))]
    pub fn accounts_impl(&self) -> Vec<Address> {
        self.state.accounts.keys().cloned().sorted().collect()
    }

    #[tracing::instrument(skip(self))]
    pub fn syncing_impl(&self) -> SyncState {
        if let Some(state) = &self.state.sync_state {
            // Node supports syncing process (i.e. not the main node).
            if state.is_synced() {
                SyncState::NotSyncing
            } else {
                SyncState::Syncing(SyncInfo {
                    starting_block: 0u64.into(), // We always start syncing from genesis right now.
                    current_block: state.get_local_block().0.into(),
                    highest_block: state.get_main_node_block().0.into(),
                })
            }
        } else {
            // If there is no sync state, then the node is the main node and it's always synced.
            SyncState::NotSyncing
        }
    }

    #[tracing::instrument(skip(self, typed_filter))]
    async fn filter_changes(
        &self,
        typed_filter: TypedFilter,
    ) -> Result<(FilterChanges, TypedFilter), Web3Error> {
        const METHOD_NAME: &str = "filter_changes";

        let res = match typed_filter {
            TypedFilter::Blocks(from_block) => {
                let (block_hashes, last_block_number) = self
                    .state
                    .connection_pool
                    .access_storage_tagged("api")
                    .await
                    .blocks_web3_dal()
                    .get_block_hashes_after(from_block, self.state.api_config.req_entities_limit)
                    .await
                    .map_err(|err| internal_error(METHOD_NAME, err))?;
                (
                    FilterChanges::Hashes(block_hashes),
                    TypedFilter::Blocks(last_block_number.unwrap_or(from_block)),
                )
            }
            TypedFilter::PendingTransactions(from_timestamp) => {
                let (tx_hashes, last_timestamp) = self
                    .state
                    .connection_pool
                    .access_storage_tagged("api")
                    .await
                    .transactions_web3_dal()
                    .get_pending_txs_hashes_after(
                        from_timestamp,
                        Some(self.state.api_config.req_entities_limit),
                    )
                    .await
                    .map_err(|err| internal_error(METHOD_NAME, err))?;
                (
                    FilterChanges::Hashes(tx_hashes),
                    TypedFilter::PendingTransactions(last_timestamp.unwrap_or(from_timestamp)),
                )
            }
            TypedFilter::Events(filter, from_block) => {
                let addresses: Vec<_> = filter
                    .address
                    .clone()
                    .into_iter()
                    .flat_map(|v| v.0)
                    .collect();
                if let Some(topics) = filter.topics.as_ref() {
                    if topics.len() > EVENT_TOPIC_NUMBER_LIMIT {
                        return Err(Web3Error::TooManyTopics);
                    }
                }
                let topics: Vec<_> = filter
                    .topics
                    .clone()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(idx, topics)| topics.map(|topics| (idx as u32 + 1, topics.0)))
                    .collect();
                let get_logs_filter = GetLogsFilter {
                    from_block,
                    to_block: filter.to_block,
                    addresses,
                    topics,
                };
                let to_block = self
                    .state
                    .resolve_filter_block_number(filter.to_block)
                    .await?;

                let mut storage = self
                    .state
                    .connection_pool
                    .access_storage_tagged("api")
                    .await;

                // Check if there is more than one block in range and there are more than `req_entities_limit` logs that satisfies filter.
                // In this case we should return error and suggest requesting logs with smaller block range.
                if from_block != to_block {
                    if let Some(miniblock_number) = storage
                        .events_web3_dal()
                        .get_log_block_number(
                            get_logs_filter.clone(),
                            self.state.api_config.req_entities_limit,
                        )
                        .await
                        .map_err(|err| internal_error(METHOD_NAME, err))?
                    {
                        return Err(Web3Error::LogsLimitExceeded(
                            self.state.api_config.req_entities_limit,
                            from_block.0,
                            miniblock_number.0 - 1,
                        ));
                    }
                }

                let logs = storage
                    .events_web3_dal()
                    .get_logs(get_logs_filter, i32::MAX as usize)
                    .await
                    .map_err(|err| internal_error(METHOD_NAME, err))?;
                let new_from_block = logs
                    .last()
                    .map(|log| MiniblockNumber(log.block_number.unwrap().as_u32()))
                    .unwrap_or(from_block);
                (
                    FilterChanges::Logs(logs),
                    TypedFilter::Events(filter, new_from_block),
                )
            }
        };

        Ok(res)
    }

    #[cfg(feature = "openzeppelin_tests")]
    pub fn send_transaction_impl(
        &self,
        transaction_request: zksync_types::web3::types::TransactionRequest,
    ) -> Result<H256, Web3Error> {
        let nonce = if let Some(nonce) = transaction_request.nonce {
            nonce
        } else {
            self.state
                .connection_pool
                .access_storage_tagged("api")
                .await
                .transactions_web3_dal()
                .next_nonce_by_initiator_account(transaction_request.from)
                .map_err(|err| internal_error("send_transaction", err))?
        };
        let mut eip712_meta = Eip712Meta::default();
        eip712_meta.gas_per_pubdata = U256::from(MAX_GAS_PER_PUBDATA_BYTE);
        let fair_l2_gas_price = self.state.tx_sender.0.state_keeper_config.fair_l2_gas_price;
        let transaction_request = TransactionRequest {
            nonce,
            from: Some(transaction_request.from),
            to: transaction_request.to,
            value: transaction_request.value.unwrap_or(U256::from(0)),
            gas_price: U256::from(fair_l2_gas_price),
            gas: transaction_request.gas.unwrap(),
            max_priority_fee_per_gas: Some(U256::from(fair_l2_gas_price)),
            input: transaction_request.data.unwrap_or_default(),
            v: None,
            r: None,
            s: None,
            raw: None,
            transaction_type: Some(EIP_712_TX_TYPE.into()),
            access_list: None,
            eip712_meta: Some(eip712_meta),
            chain_id: None,
        };
        let transaction_request = self.convert_evm_like_deploy_requests(transaction_request)?;

        let bytes = if let Some(signer) = transaction_request
            .from
            .and_then(|from| self.state.accounts.get(&from).cloned())
        {
            let chain_id = self.state.api_config.l2_chain_id;
            let domain = Eip712Domain::new(chain_id);
            let signature = signer
                .sign_typed_data(&domain, &transaction_request)
                .await
                .map_err(|err| internal_error("send_transaction", err))?;

            let encoded_tx = transaction_request.get_signed_bytes(&signature, chain_id);
            Bytes(encoded_tx)
        } else {
            return Err(internal_error("send_transaction", "Account not found"));
        };

        self.send_raw_transaction_impl(bytes)
    }

    #[cfg(feature = "openzeppelin_tests")]
    /// Converts EVM-like transaction requests of deploying contracts to zkEVM format.
    /// These feature is needed to run openzeppelin tests
    /// because they use `truffle` which uses `web3.js` to generate transaction requests.
    /// Note, that we can remove this method when ZkSync support
    /// will be added for `truffle`.
    fn convert_evm_like_deploy_requests(
        &self,
        mut transaction_request: TransactionRequest,
    ) -> Result<TransactionRequest, Web3Error> {
        if transaction_request.to.unwrap_or(Address::zero()) == Address::zero() {
            transaction_request.to = Some(CONTRACT_DEPLOYER_ADDRESS);
            transaction_request.transaction_type = Some(EIP_712_TX_TYPE.into());

            const BYTECODE_CHUNK_LEN: usize = 32;

            let data = transaction_request.input.0;
            let (bytecode, constructor_calldata) =
                data.split_at(data.len() / BYTECODE_CHUNK_LEN * BYTECODE_CHUNK_LEN);
            let mut bytecode = bytecode.to_vec();
            let mut constructor_calldata = constructor_calldata.to_vec();
            let lock = self.state.known_bytecodes.read().unwrap();
            while !lock.contains(&bytecode) {
                if bytecode.len() < BYTECODE_CHUNK_LEN {
                    return Err(internal_error(
                        "convert_evm_like_deploy_requests",
                        "Bytecode not found",
                    ));
                }
                let (new_bytecode, new_constructor_part) =
                    bytecode.split_at(bytecode.len() - BYTECODE_CHUNK_LEN);
                constructor_calldata = new_constructor_part
                    .iter()
                    .chain(constructor_calldata.iter())
                    .cloned()
                    .collect();
                bytecode = new_bytecode.to_vec();
            }
            drop(lock);

            let mut eip712_meta = Eip712Meta::default();
            eip712_meta.gas_per_pubdata = U256::from(MAX_GAS_PER_PUBDATA_BYTE);
            eip712_meta.factory_deps = Some(vec![bytecode.clone()]);
            transaction_request.eip712_meta = Some(eip712_meta);

            let salt = H256::zero();
            let bytecode_hash = hash_bytecode(&bytecode);

            let deployer = zksync_contracts::deployer_contract();
            transaction_request.input = Bytes(
                deployer
                    .function("create")
                    .unwrap()
                    .encode_input(&[
                        salt.into_token(),
                        bytecode_hash.into_token(),
                        constructor_calldata.into_token(),
                    ])
                    .unwrap(),
            );
        }
        Ok(transaction_request)
    }
}

// Bogus methods.
// They are moved into a separate `impl` block so they don't make the actual implementation noisy.
// This `impl` block contains methods that we *have* to implement for compliance, but don't really
// make sense in terms in L2.
impl<E> EthNamespace<E> {
    pub fn coinbase_impl(&self) -> Address {
        // There is no coinbase account.
        Address::default()
    }

    pub fn compilers_impl(&self) -> Vec<String> {
        // This node doesn't support compilation.
        Vec::new()
    }

    pub fn uncle_count_impl(&self, _block: BlockId) -> Option<U256> {
        // We don't have uncles in zkSync.
        Some(0.into())
    }

    pub fn hashrate_impl(&self) -> U256 {
        // zkSync is not a PoW chain.
        U256::zero()
    }

    pub fn mining_impl(&self) -> bool {
        // zkSync is not a PoW chain.
        false
    }

    // List of methods that are not supported at all:
    //
    // - `sign`.
    // - `submit_hashrate`.
    // - `submit_work`.
    // - `compile_lll`.
    // - `compile_solidity`.
    // - `compile_serpent`.
}

use crate::{
    database::OnChainIterableKeyValueView,
    service::{
        adapters::{
            BlockProducerAdapter,
            ChainStateInfoProvider,
            ExecutorAdapter,
            MaybeRelayerAdapter,
            NewTxWaiter,
            StaticGasPrice,
            TransactionsSource,
            TxPoolAdapter,
        },
        sub_services::BlockProducerService,
    },
};
use fuel_core_producer::{
    block_producer::gas_price::{
        ChainStateInfoProvider as ChainStateInfoProviderTrait,
        GasPriceProvider,
    },
    ports::{
        RelayerBlockInfo,
        TxPool,
    },
};
use fuel_core_storage::{
    iter::{
        IterDirection,
        IteratorOverTable,
    },
    not_found,
    tables::{
        ConsensusParametersVersions,
        FuelBlocks,
        StateTransitionBytecodeVersions,
        Transactions,
    },
    transactional::Changes,
    Result as StorageResult,
    StorageAsRef,
};
use fuel_core_types::{
    blockchain::{
        block::{
            Block,
            CompressedBlock,
        },
        header::{
            ConsensusParametersVersion,
            StateTransitionBytecodeVersion,
        },
        primitives::DaBlockHeight,
    },
    fuel_tx::{
        ConsensusParameters,
        Transaction,
    },
    fuel_types::{
        BlockHeight,
        Bytes32,
    },
    services::{
        block_producer::Components,
        executor::{
            DryRunResult,
            Result as ExecutorResult,
            StorageReadReplayEvent,
            UncommittedResult,
        },
    },
};
use std::{
    borrow::Cow,
    sync::Arc,
};
use tokio::time::Instant;

impl BlockProducerAdapter {
    pub fn new(block_producer: BlockProducerService) -> Self {
        Self {
            block_producer: Arc::new(block_producer),
        }
    }
}

impl TxPool for TxPoolAdapter {
    type TxSource = TransactionsSource;

    async fn get_source(
        &self,
        gas_price: u64,
        _: BlockHeight,
    ) -> anyhow::Result<Self::TxSource> {
        Ok(TransactionsSource::new(gas_price, self.service.clone()))
    }
}

impl fuel_core_producer::ports::BlockProducer<TransactionsSource> for ExecutorAdapter {
    type Deadline = Instant;
    async fn produce_without_commit(
        &self,
        component: Components<TransactionsSource>,
        deadline: Instant,
    ) -> ExecutorResult<UncommittedResult<Changes>> {
        let new_tx_waiter = NewTxWaiter::new(self.new_txs_watcher.clone(), deadline);
        self.executor
            .produce_without_commit_with_source(
                component,
                new_tx_waiter,
                self.preconfirmation_sender.clone(),
            )
            .await
    }
}

impl fuel_core_producer::ports::BlockProducer<Vec<Transaction>> for ExecutorAdapter {
    type Deadline = ();
    async fn produce_without_commit(
        &self,
        component: Components<Vec<Transaction>>,
        _: (),
    ) -> ExecutorResult<UncommittedResult<Changes>> {
        self.produce_without_commit_from_vector(component)
    }
}

impl fuel_core_producer::ports::DryRunner for ExecutorAdapter {
    fn dry_run(
        &self,
        block: Components<Vec<Transaction>>,
        forbid_fake_coins: Option<bool>,
        at_height: Option<BlockHeight>,
        record_storage_read_replay: bool,
    ) -> ExecutorResult<DryRunResult> {
        self.executor.dry_run(
            block,
            forbid_fake_coins,
            at_height,
            record_storage_read_replay,
        )
    }
}

impl fuel_core_producer::ports::StorageReadReplayRecorder for ExecutorAdapter {
    fn storage_read_replay(
        &self,
        block: &Block,
    ) -> ExecutorResult<Vec<StorageReadReplayEvent>> {
        self.executor.storage_read_replay(block)
    }
}

#[async_trait::async_trait]
impl fuel_core_producer::ports::Relayer for MaybeRelayerAdapter {
    async fn wait_for_at_least_height(
        &self,
        height: &DaBlockHeight,
    ) -> anyhow::Result<DaBlockHeight> {
        #[cfg(feature = "relayer")]
        {
            if let Some(sync) = &self.relayer_synced {
                sync.await_at_least_synced(height).await?;
                let highest = sync.get_finalized_da_height();
                Ok(highest)
            } else {
                Ok(*height)
            }
        }
        #[cfg(not(feature = "relayer"))]
        {
            anyhow::ensure!(
                **height == 0,
                "Cannot have a da height above zero without a relayer"
            );
            // If the relayer is not enabled, then all blocks are zero.
            Ok(0u64.into())
        }
    }

    async fn get_cost_and_transactions_number_for_block(
        &self,
        height: &DaBlockHeight,
    ) -> anyhow::Result<RelayerBlockInfo> {
        #[cfg(feature = "relayer")]
        {
            let (gas_cost, tx_count) = self
                .relayer_database
                .get_events(height)?
                .iter()
                .fold((0u64, 0u64), |(gas_cost, tx_count), event| {
                    let gas_cost = gas_cost.saturating_add(event.cost());
                    let tx_count = match event {
                        fuel_core_types::services::relayer::Event::Message(_) => tx_count,
                        fuel_core_types::services::relayer::Event::Transaction(_) => {
                            tx_count.saturating_add(1)
                        }
                    };
                    (gas_cost, tx_count)
                });
            Ok(RelayerBlockInfo { gas_cost, tx_count })
        }
        #[cfg(not(feature = "relayer"))]
        {
            anyhow::ensure!(
                **height == 0,
                "Cannot have a da height above zero without a relayer"
            );
            // If the relayer is not enabled, then all blocks are zero.
            Ok(RelayerBlockInfo {
                gas_cost: 0,
                tx_count: 0,
            })
        }
    }
}

impl fuel_core_producer::ports::BlockProducerDatabase for OnChainIterableKeyValueView {
    fn latest_height(&self) -> Option<BlockHeight> {
        self.latest_height().ok()
    }

    fn get_block(&self, height: &BlockHeight) -> StorageResult<Cow<CompressedBlock>> {
        self.storage::<FuelBlocks>()
            .get(height)?
            .ok_or(not_found!(FuelBlocks))
    }

    fn get_full_block(&self, height: &BlockHeight) -> StorageResult<Block> {
        let block = self.get_block(height)?;
        let transactions = block
            .transactions()
            .iter()
            .map(|id| {
                self.storage::<Transactions>()
                    .get(id)?
                    .ok_or(not_found!(Transactions))
                    .map(|tx| tx.into_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(block.into_owned().uncompress(transactions))
    }

    fn block_header_merkle_root(&self, height: &BlockHeight) -> StorageResult<Bytes32> {
        self.storage::<FuelBlocks>().root(height).map(Into::into)
    }

    fn latest_consensus_parameters_version(
        &self,
    ) -> StorageResult<ConsensusParametersVersion> {
        let (version, _) = self
            .iter_all::<ConsensusParametersVersions>(Some(IterDirection::Reverse))
            .next()
            .ok_or(not_found!(ConsensusParametersVersions))??;

        Ok(version)
    }

    fn latest_state_transition_bytecode_version(
        &self,
    ) -> StorageResult<StateTransitionBytecodeVersion> {
        let (version, _) = self
            .iter_all::<StateTransitionBytecodeVersions>(Some(IterDirection::Reverse))
            .next()
            .ok_or(not_found!(StateTransitionBytecodeVersions))??;

        Ok(version)
    }
}

impl GasPriceProvider for StaticGasPrice {
    fn production_gas_price(&self) -> anyhow::Result<u64> {
        Ok(self.gas_price)
    }

    fn dry_run_gas_price(&self) -> anyhow::Result<u64> {
        Ok(self.gas_price)
    }
}

impl ChainStateInfoProviderTrait for ChainStateInfoProvider {
    fn consensus_params_at_version(
        &self,
        version: &ConsensusParametersVersion,
    ) -> anyhow::Result<Arc<ConsensusParameters>> {
        Ok(self.shared_state.get_consensus_parameters(version)?)
    }
}

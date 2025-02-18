use crate::fraud_proof::{find_trace_mismatch, FraudProofGenerator};
use crate::parent_chain::ParentChainInterface;
use crate::utils::{
    to_number_primitive, DomainBlockImportNotification, DomainImportNotificationSinks,
};
use crate::ExecutionReceiptFor;
use codec::{Decode, Encode};
use domain_block_builder::{BlockBuilder, BuiltBlock, RecordProof};
use domain_runtime_primitives::DomainCoreApi;
use sc_client_api::{AuxStore, BlockBackend, Finalizer, StateBackendFor, TransactionFor};
use sc_consensus::{
    BlockImport, BlockImportParams, ForkChoiceStrategy, ImportResult, StateAction, StorageChanges,
};
use sp_api::{NumberFor, ProvideRuntimeApi};
use sp_blockchain::{HashAndNumber, HeaderBackend, HeaderMetadata};
use sp_consensus::{BlockOrigin, SyncOracle};
use sp_core::traits::CodeExecutor;
use sp_domains::fraud_proof::FraudProof;
use sp_domains::merkle_tree::MerkleTree;
use sp_domains::{DomainId, ExecutionReceipt, ExecutorApi};
use sp_runtime::traits::{Block as BlockT, CheckedSub, HashFor, Header as HeaderT, One, Zero};
use sp_runtime::Digest;
use std::sync::Arc;

pub(crate) struct DomainBlockResult<Block, PBlock>
where
    Block: BlockT,
    PBlock: BlockT,
{
    pub header_hash: Block::Hash,
    pub header_number: NumberFor<Block>,
    pub execution_receipt: ExecutionReceiptFor<PBlock, Block::Hash>,
}

/// An abstracted domain block processor.
pub(crate) struct DomainBlockProcessor<Block, PBlock, Client, PClient, Backend, BI>
where
    Block: BlockT,
    PBlock: BlockT,
{
    pub(crate) domain_id: DomainId,
    pub(crate) client: Arc<Client>,
    pub(crate) primary_chain_client: Arc<PClient>,
    pub(crate) backend: Arc<Backend>,
    pub(crate) domain_confirmation_depth: NumberFor<Block>,
    pub(crate) block_import: Arc<BI>,
    pub(crate) import_notification_sinks: DomainImportNotificationSinks<Block, PBlock>,
}

impl<Block, PBlock, Client, PClient, Backend, BI> Clone
    for DomainBlockProcessor<Block, PBlock, Client, PClient, Backend, BI>
where
    Block: BlockT,
    PBlock: BlockT,
{
    fn clone(&self) -> Self {
        Self {
            domain_id: self.domain_id,
            client: self.client.clone(),
            primary_chain_client: self.primary_chain_client.clone(),
            backend: self.backend.clone(),
            domain_confirmation_depth: self.domain_confirmation_depth,
            block_import: self.block_import.clone(),
            import_notification_sinks: self.import_notification_sinks.clone(),
        }
    }
}

/// A list of primary blocks waiting to be processed by executor on each imported primary block
/// notification.
///
/// Usually, each new domain block is built on top of the current best domain block, with the block
/// content extracted from the incoming primary block. However, an incoming imported primary block
/// notification can also imply multiple pending primary blocks in case of the primary chain re-org.
#[derive(Debug)]
pub(crate) struct PendingPrimaryBlocks<Block: BlockT, PBlock: BlockT> {
    /// Base block used to build new domain blocks derived from the primary blocks below.
    pub initial_parent: (Block::Hash, NumberFor<Block>),
    /// Pending primary blocks that need to be processed sequentially.
    pub primary_imports: Vec<HashAndNumber<PBlock>>,
}

impl<Block, PBlock, Client, PClient, Backend, BI>
    DomainBlockProcessor<Block, PBlock, Client, PClient, Backend, BI>
where
    Block: BlockT,
    PBlock: BlockT,
    NumberFor<PBlock>: Into<NumberFor<Block>>,
    Client: HeaderBackend<Block>
        + BlockBackend<Block>
        + AuxStore
        + ProvideRuntimeApi<Block>
        + Finalizer<Block, Backend>
        + 'static,
    Client::Api: DomainCoreApi<Block>
        + sp_block_builder::BlockBuilder<Block>
        + sp_api::ApiExt<Block, StateBackend = StateBackendFor<Backend, Block>>,
    for<'b> &'b BI: BlockImport<
        Block,
        Transaction = sp_api::TransactionFor<Client, Block>,
        Error = sp_consensus::Error,
    >,
    PClient: HeaderBackend<PBlock>
        + HeaderMetadata<PBlock, Error = sp_blockchain::Error>
        + BlockBackend<PBlock>
        + ProvideRuntimeApi<PBlock>
        + 'static,
    PClient::Api: ExecutorApi<PBlock, Block::Hash> + 'static,
    Backend: sc_client_api::Backend<Block> + 'static,
    TransactionFor<Backend, Block>: sp_trie::HashDBT<HashFor<Block>, sp_trie::DBValue>,
{
    /// Returns a list of primary blocks waiting to be processed if any.
    ///
    /// It's possible to have multiple pending primary blocks that need to be processed in case
    /// the primary chain re-org occurs.
    pub(crate) fn pending_imported_primary_blocks(
        &self,
        primary_hash: PBlock::Hash,
        primary_number: NumberFor<PBlock>,
    ) -> sp_blockchain::Result<Option<PendingPrimaryBlocks<Block, PBlock>>> {
        if primary_number == One::one() {
            return Ok(Some(PendingPrimaryBlocks {
                initial_parent: (self.client.info().genesis_hash, Zero::zero()),
                primary_imports: vec![HashAndNumber {
                    hash: primary_hash,
                    number: primary_number,
                }],
            }));
        }

        let best_hash = self.client.info().best_hash;
        let best_number = self.client.info().best_number;

        let primary_hash_for_best_domain_hash =
            crate::aux_schema::primary_hash_for(&*self.backend, best_hash)?.ok_or_else(|| {
                sp_blockchain::Error::Backend(format!(
                    "Primary hash for domain hash #{best_number},{best_hash} not found"
                ))
            })?;

        let primary_from = primary_hash_for_best_domain_hash;
        let primary_to = primary_hash;

        if primary_from == primary_to {
            return Err(sp_blockchain::Error::Application(Box::from(
                "Primary block {primary_hash:?} has already been processed.",
            )));
        }

        let route =
            sp_blockchain::tree_route(&*self.primary_chain_client, primary_from, primary_to)?;

        let retracted = route.retracted();
        let enacted = route.enacted();

        tracing::trace!(
            ?retracted,
            ?enacted,
            common_block = ?route.common_block(),
            "Calculating PendingPrimaryBlocks on #{best_number},{best_hash:?}"
        );

        match (retracted.is_empty(), enacted.is_empty()) {
            (true, false) => {
                // New tip, A -> B
                Ok(Some(PendingPrimaryBlocks {
                    initial_parent: (best_hash, best_number),
                    primary_imports: enacted.to_vec(),
                }))
            }
            (false, true) => {
                tracing::debug!("Primary blocks {retracted:?} have been already processed");
                Ok(None)
            }
            (true, true) => {
                unreachable!(
                    "Tree route is not empty as `primary_from` and `primary_to` in tree_route() \
                    are checked above to be not the same; qed",
                );
            }
            (false, false) => {
                let common_block_number = route.common_block().number.into();
                let parent_header = self
                    .client
                    .header(self.client.hash(common_block_number)?.ok_or_else(|| {
                        sp_blockchain::Error::Backend(format!(
                            "Header for #{common_block_number} not found"
                        ))
                    })?)?
                    .ok_or_else(|| {
                        sp_blockchain::Error::Backend(format!(
                            "Header for #{common_block_number} not found"
                        ))
                    })?;

                Ok(Some(PendingPrimaryBlocks {
                    initial_parent: (parent_header.hash(), *parent_header.number()),
                    primary_imports: enacted.to_vec(),
                }))
            }
        }
    }

    pub(crate) async fn process_domain_block(
        &self,
        (primary_hash, primary_number): (PBlock::Hash, NumberFor<PBlock>),
        (parent_hash, parent_number): (Block::Hash, NumberFor<Block>),
        extrinsics: Vec<Block::Extrinsic>,
        digests: Digest,
    ) -> Result<DomainBlockResult<Block, PBlock>, sp_blockchain::Error> {
        let primary_number = to_number_primitive(primary_number);

        if to_number_primitive(parent_number) + 1 != primary_number {
            return Err(sp_blockchain::Error::Application(Box::from(format!(
                "Wrong domain parent block #{parent_number},{parent_hash} for \
                primary block #{primary_number},{primary_hash}, the number of new \
                domain block must match the number of corresponding primary block."
            ))));
        }

        // Although the domain block intuitively ought to use the same fork choice
        // from the corresponding primary block, it's fine to forcibly always use
        // the longest chain for simplicity as we manually build all the domain
        // branches by literally following the primary chain branches anyway.
        let fork_choice = ForkChoiceStrategy::LongestChain;

        let (header_hash, header_number, state_root) = self
            .build_and_import_block(parent_hash, parent_number, extrinsics, fork_choice, digests)
            .await?;

        tracing::debug!(
            "Built new domain block #{header_number},{header_hash} from primary block #{primary_number},{primary_hash} \
            on top of parent block #{parent_number},{parent_hash}"
        );

        if let Some(to_finalize_block_number) =
            header_number.checked_sub(&self.domain_confirmation_depth)
        {
            if to_finalize_block_number > self.client.info().finalized_number {
                let to_finalize_block_hash =
                    self.client.hash(to_finalize_block_number)?.ok_or_else(|| {
                        sp_blockchain::Error::Backend(format!(
                            "Header for #{to_finalize_block_number} not found"
                        ))
                    })?;
                self.client
                    .finalize_block(to_finalize_block_hash, None, true)?;
                tracing::debug!("Successfully finalized block: #{to_finalize_block_number},{to_finalize_block_hash}");
            }
        }

        let mut roots = self.client.runtime_api().intermediate_roots(header_hash)?;

        let state_root = state_root
            .encode()
            .try_into()
            .expect("State root uses the same Block hash type which must fit into [u8; 32]; qed");

        roots.push(state_root);

        let trace_root = MerkleTree::from_leaves(&roots).root().ok_or_else(|| {
            sp_blockchain::Error::Application(Box::from("Failed to get merkle root of trace"))
        })?;
        let trace = roots
            .into_iter()
            .map(|r| {
                Block::Hash::decode(&mut r.as_slice())
                    .expect("Storage root uses the same Block hash type; qed")
            })
            .collect();

        tracing::trace!(
            ?trace,
            ?trace_root,
            "Trace root calculated for #{header_number},{header_hash}"
        );

        let execution_receipt = ExecutionReceipt {
            primary_number: primary_number.into(),
            primary_hash,
            domain_hash: header_hash,
            trace,
            trace_root,
        };

        Ok(DomainBlockResult {
            header_hash,
            header_number,
            execution_receipt,
        })
    }

    async fn build_and_import_block(
        &self,
        parent_hash: Block::Hash,
        parent_number: NumberFor<Block>,
        extrinsics: Vec<Block::Extrinsic>,
        fork_choice: ForkChoiceStrategy,
        digests: Digest,
    ) -> Result<(Block::Hash, NumberFor<Block>, Block::Hash), sp_blockchain::Error> {
        let block_builder = BlockBuilder::new(
            &*self.client,
            parent_hash,
            parent_number,
            RecordProof::No,
            digests,
            &*self.backend,
            extrinsics,
        )?;

        let BuiltBlock {
            block,
            storage_changes,
            proof: _,
        } = block_builder.build()?;

        let (header, body) = block.deconstruct();
        let state_root = *header.state_root();
        let header_hash = header.hash();
        let header_number = *header.number();

        let block_import_params = {
            let mut import_block = BlockImportParams::new(BlockOrigin::Own, header);
            import_block.body = Some(body);
            import_block.state_action =
                StateAction::ApplyChanges(StorageChanges::Changes(storage_changes));
            // Follow the primary block's fork choice.
            import_block.fork_choice = Some(fork_choice);
            import_block
        };

        let import_result = (&*self.block_import)
            .import_block(block_import_params)
            .await?;

        match import_result {
            ImportResult::Imported(..) => {}
            ImportResult::AlreadyInChain => {
                tracing::debug!("Block #{header_number},{header_hash:?} is already in chain");
            }
            ImportResult::KnownBad => {
                return Err(sp_consensus::Error::ClientImport(format!(
                    "Bad block #{header_number}({header_hash:?})"
                ))
                .into());
            }
            ImportResult::UnknownParent => {
                return Err(sp_consensus::Error::ClientImport(format!(
                    "Block #{header_number}({header_hash:?}) has an unknown parent: {parent_hash:?}"
                ))
                .into());
            }
            ImportResult::MissingState => {
                return Err(sp_consensus::Error::ClientImport(format!(
                    "Parent state of block #{header_number}({header_hash:?}) is missing, parent: {parent_hash:?}"
                ))
                    .into());
            }
        }

        Ok((header_hash, header_number, state_root))
    }

    pub(crate) fn on_domain_block_processed(
        &self,
        primary_hash: PBlock::Hash,
        domain_block_result: DomainBlockResult<Block, PBlock>,
        head_receipt_number: NumberFor<Block>,
    ) -> sp_blockchain::Result<()> {
        let DomainBlockResult {
            header_hash,
            header_number: _,
            execution_receipt,
        } = domain_block_result;

        crate::aux_schema::write_execution_receipt::<_, Block, PBlock>(
            &*self.client,
            head_receipt_number,
            &execution_receipt,
        )?;

        crate::aux_schema::track_domain_hash_to_primary_hash(
            &*self.client,
            header_hash,
            primary_hash,
        )?;

        // Notify the imported domain block when the receipt processing is done.
        let domain_import_notification = DomainBlockImportNotification {
            domain_block_hash: header_hash,
            primary_block_hash: primary_hash,
        };
        self.import_notification_sinks.lock().retain(|sink| {
            sink.unbounded_send(domain_import_notification.clone())
                .is_ok()
        });

        Ok(())
    }
}

pub(crate) struct ReceiptsChecker<
    Block,
    Client,
    PBlock,
    PClient,
    Backend,
    E,
    ParentChain,
    ParentChainBlock,
> {
    pub(crate) domain_id: DomainId,
    pub(crate) client: Arc<Client>,
    pub(crate) primary_chain_client: Arc<PClient>,
    pub(crate) primary_network_sync_oracle: Arc<dyn SyncOracle + Send + Sync>,
    pub(crate) fraud_proof_generator:
        FraudProofGenerator<Block, PBlock, Client, PClient, Backend, E>,
    pub(crate) parent_chain: ParentChain,
    pub(crate) _phantom: std::marker::PhantomData<ParentChainBlock>,
}

impl<Block, PBlock, Client, PClient, Backend, E, ParentChain, ParentChainBlock> Clone
    for ReceiptsChecker<Block, PBlock, Client, PClient, Backend, E, ParentChain, ParentChainBlock>
where
    Block: BlockT,
    ParentChain: Clone,
{
    fn clone(&self) -> Self {
        Self {
            domain_id: self.domain_id,
            client: self.client.clone(),
            primary_chain_client: self.primary_chain_client.clone(),
            primary_network_sync_oracle: self.primary_network_sync_oracle.clone(),
            fraud_proof_generator: self.fraud_proof_generator.clone(),
            parent_chain: self.parent_chain.clone(),
            _phantom: self._phantom,
        }
    }
}

impl<Block, Client, PBlock, PClient, Backend, E, ParentChain, ParentChainBlock>
    ReceiptsChecker<Block, Client, PBlock, PClient, Backend, E, ParentChain, ParentChainBlock>
where
    Block: BlockT,
    PBlock: BlockT,
    ParentChainBlock: BlockT,
    NumberFor<PBlock>: Into<NumberFor<Block>>,
    Client:
        HeaderBackend<Block> + BlockBackend<Block> + AuxStore + ProvideRuntimeApi<Block> + 'static,
    Client::Api: DomainCoreApi<Block>
        + sp_block_builder::BlockBuilder<Block>
        + sp_api::ApiExt<Block, StateBackend = StateBackendFor<Backend, Block>>,
    PClient: HeaderBackend<PBlock> + BlockBackend<PBlock> + ProvideRuntimeApi<PBlock> + 'static,
    PClient::Api: ExecutorApi<PBlock, Block::Hash>,
    Backend: sc_client_api::Backend<Block> + 'static,
    TransactionFor<Backend, Block>: sp_trie::HashDBT<HashFor<Block>, sp_trie::DBValue>,
    E: CodeExecutor,
    ParentChain: ParentChainInterface<Block, ParentChainBlock>,
{
    pub(crate) fn check_state_transition(
        &self,
        parent_chain_block_hash: ParentChainBlock::Hash,
    ) -> sp_blockchain::Result<()> {
        let extrinsics = self.parent_chain.block_body(parent_chain_block_hash)?;

        let receipts = self
            .parent_chain
            .extract_receipts(parent_chain_block_hash, extrinsics.clone())?;

        let fraud_proofs = self
            .parent_chain
            .extract_fraud_proofs(parent_chain_block_hash, extrinsics)?;

        self.check_receipts(receipts, fraud_proofs)?;

        if self.primary_network_sync_oracle.is_major_syncing() {
            tracing::debug!(
                "Skip reporting unconfirmed bad receipt as the primary node is still major syncing..."
            );
            return Ok(());
        }

        // Submit fraud proof for the first unconfirmed incorrent ER.
        let oldest_receipt_number = self
            .parent_chain
            .oldest_receipt_number(parent_chain_block_hash)?;
        crate::aux_schema::prune_expired_bad_receipts(&*self.client, oldest_receipt_number)?;

        if let Some(fraud_proof) = self.create_fraud_proof_for_first_unconfirmed_bad_receipt()? {
            self.parent_chain.submit_fraud_proof_unsigned(fraud_proof)?;
        }

        Ok(())
    }

    fn check_receipts(
        &self,
        receipts: Vec<ExecutionReceiptFor<ParentChainBlock, Block::Hash>>,
        fraud_proofs: Vec<FraudProof<NumberFor<ParentChainBlock>, ParentChainBlock::Hash>>,
    ) -> Result<(), sp_blockchain::Error> {
        let mut bad_receipts_to_write = vec![];

        for execution_receipt in receipts.iter() {
            let primary_block_hash = execution_receipt.primary_hash;

            let local_receipt = crate::aux_schema::load_execution_receipt::<
                _,
                Block::Hash,
                NumberFor<Block>,
                ParentChainBlock::Hash,
            >(&*self.client, primary_block_hash)?
            .ok_or(sp_blockchain::Error::Backend(format!(
                "receipt for primary block #{},{primary_block_hash} not found",
                execution_receipt.primary_number
            )))?;

            if let Some(trace_mismatch_index) =
                find_trace_mismatch(&local_receipt.trace, &execution_receipt.trace)
            {
                bad_receipts_to_write.push((
                    execution_receipt.primary_number,
                    execution_receipt.hash(),
                    (trace_mismatch_index, primary_block_hash),
                ));
            }
        }

        let bad_receipts_to_delete = fraud_proofs
            .into_iter()
            .filter_map(|fraud_proof| {
                match fraud_proof {
                    FraudProof::InvalidStateTransition(fraud_proof) => {
                        let bad_receipt_number = fraud_proof.parent_number + 1;
                        let bad_receipt_hash = fraud_proof.bad_receipt_hash;

                        // In order to not delete a receipt which was just inserted, accumulate the write&delete operations
                        // in case the bad receipt and corresponding farud proof are included in the same block.
                        if let Some(index) = bad_receipts_to_write
                            .iter()
                            .map(|(_, receipt_hash, _)| receipt_hash)
                            .position(|v| *v == bad_receipt_hash)
                        {
                            bad_receipts_to_write.swap_remove(index);
                            None
                        } else {
                            Some((bad_receipt_number, bad_receipt_hash))
                        }
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();

        for (bad_receipt_number, bad_receipt_hash, mismatch_info) in bad_receipts_to_write {
            crate::aux_schema::write_bad_receipt::<_, ParentChainBlock>(
                &*self.client,
                bad_receipt_number,
                bad_receipt_hash,
                mismatch_info,
            )?;
        }

        for (bad_receipt_number, bad_receipt_hash) in bad_receipts_to_delete {
            if let Err(e) = crate::aux_schema::delete_bad_receipt(
                &*self.client,
                bad_receipt_number,
                bad_receipt_hash,
            ) {
                tracing::error!(
                    error = ?e,
                    ?bad_receipt_number,
                    ?bad_receipt_hash,
                    "Failed to delete bad receipt"
                );
            }
        }

        Ok(())
    }

    fn create_fraud_proof_for_first_unconfirmed_bad_receipt(
        &self,
    ) -> sp_blockchain::Result<
        Option<FraudProof<NumberFor<ParentChainBlock>, ParentChainBlock::Hash>>,
    > {
        if let Some((bad_receipt_hash, trace_mismatch_index, primary_block_hash)) =
            crate::aux_schema::find_first_unconfirmed_bad_receipt_info::<_, Block, PBlock, _>(
                &*self.client,
                |height| {
                    self.primary_chain_client.hash(height)?.ok_or_else(|| {
                        sp_blockchain::Error::Backend(format!(
                            "Primary block hash for {height} not found",
                        ))
                    })
                },
            )?
        {
            let local_receipt =
                crate::aux_schema::load_execution_receipt(&*self.client, primary_block_hash)?
                    .ok_or_else(|| {
                        sp_blockchain::Error::Backend(format!(
                            "Receipt for primary block {primary_block_hash} not found"
                        ))
                    })?;

            let fraud_proof = self
                .fraud_proof_generator
                .generate_invalid_state_transition_proof::<ParentChainBlock>(
                    self.domain_id,
                    trace_mismatch_index,
                    &local_receipt,
                    bad_receipt_hash,
                )
                .map_err(|err| {
                    sp_blockchain::Error::Application(Box::from(format!(
                        "Failed to generate fraud proof: {err}"
                    )))
                })?;

            return Ok(Some(fraud_proof));
        }

        Ok(None)
    }
}

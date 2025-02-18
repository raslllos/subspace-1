use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use sc_client_api::backend::Backend;
use sc_client_api::BlockImportOperation;
use sc_executor::RuntimeVersionOf;
use sc_service::{resolve_state_version_from_wasm, BuildGenesisBlock};
use sp_core::storage::{StateVersion, Storage};
use sp_runtime::traits::{Block as BlockT, Hash as HashT, Header as HeaderT, Zero};
use sp_runtime::{BuildStorage, Digest, DigestItem};
use std::marker::PhantomData;
use std::sync::Arc;
use subspace_core_primitives::RecordedHistorySegment;

/// Custom genesis block builder for Subspace.
pub struct SubspaceGenesisBlockBuilder<Block: BlockT, B, E> {
    genesis_storage: Storage,
    commit_genesis_state: bool,
    backend: Arc<B>,
    executor: E,
    _phantom: PhantomData<Block>,
}

impl<Block: BlockT, B: Backend<Block>, E: RuntimeVersionOf>
    SubspaceGenesisBlockBuilder<Block, B, E>
{
    /// Constructs a new instance of [`SubspaceGenesisBlockBuilder`].
    pub fn new(
        build_genesis_storage: &dyn BuildStorage,
        commit_genesis_state: bool,
        backend: Arc<B>,
        executor: E,
    ) -> sp_blockchain::Result<Self> {
        let genesis_storage = build_genesis_storage
            .build_storage()
            .map_err(sp_blockchain::Error::Storage)?;
        Ok(Self {
            genesis_storage,
            commit_genesis_state,
            backend,
            executor,
            _phantom: PhantomData::<Block>,
        })
    }
}

impl<Block: BlockT, B: Backend<Block>, E: RuntimeVersionOf> BuildGenesisBlock<Block>
    for SubspaceGenesisBlockBuilder<Block, B, E>
{
    type BlockImportOperation = <B as Backend<Block>>::BlockImportOperation;

    fn build_genesis_block(self) -> sp_blockchain::Result<(Block, Self::BlockImportOperation)> {
        let Self {
            genesis_storage,
            commit_genesis_state,
            backend,
            executor,
            _phantom,
        } = self;

        let genesis_state_version = resolve_state_version_from_wasm(&genesis_storage, &executor)?;
        let mut op = backend.begin_operation()?;
        let state_root =
            op.set_genesis_state(genesis_storage, commit_genesis_state, genesis_state_version)?;
        let genesis_block = construct_genesis_block::<Block>(state_root, genesis_state_version);

        Ok((genesis_block, op))
    }
}

/// Create a custom Subspace genesis block, given the initial storage.
///
/// We have a non-empty digest in comparison to the default Substrate genesis block.
fn construct_genesis_block<Block: BlockT>(
    state_root: Block::Hash,
    state_version: StateVersion,
) -> Block {
    let extrinsics_root = <<<Block as BlockT>::Header as HeaderT>::Hashing as HashT>::trie_root(
        Vec::new(),
        state_version,
    );

    // We fill genesis block with extra data such that the very first archived
    // segment can be produced right away, bootstrapping the farming process.
    let mut ballast = vec![0; RecordedHistorySegment::SIZE];
    let mut rng = ChaCha8Rng::from_seed(
        state_root
            .as_ref()
            .try_into()
            .expect("State root in Subspace must be 32 bytes, panic otherwise; qed"),
    );
    rng.fill(ballast.as_mut_slice());
    let digest = Digest {
        logs: vec![DigestItem::Other(ballast)],
    };

    Block::new(
        <<Block as BlockT>::Header as HeaderT>::new(
            Zero::zero(),
            extrinsics_root,
            state_root,
            Default::default(),
            digest,
        ),
        Default::default(),
    )
}

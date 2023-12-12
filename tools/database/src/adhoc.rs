use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use borsh::{BorshDeserialize, BorshSerialize};
use indicatif::ProgressBar;
use near_chain::types::{
    ApplyTransactionsBlockContext, ApplyTransactionsChunkContext, RuntimeAdapter,
    RuntimeStorageConfig,
};
use near_epoch_manager::EpochManager;
use near_primitives::challenge::PartialState;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::receipt::Receipt;
use near_primitives::shard_layout::ShardLayout;
use near_primitives::transaction::Action;
use near_primitives::trie_key::col::NON_DELAYED_RECEIPT_COLUMNS;
use near_primitives::types::AccountId;
use near_primitives::utils::get_block_shard_id;
use near_store::db::Database;
use near_store::{DBCol, PartialStorage, RawTrieNodeWithSize, Trie, TrieCache};
use nearcore::NightshadeRuntime;

use crate::utils::open_rocksdb;

#[derive(clap::Parser)]
pub struct AdHocCommand {
    /// Desired DbKind.
    #[clap(long)]
    action: String,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct AllData {
    blocks: Vec<Block>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Block {
    block: near_primitives::block::Block,
    chunks: Vec<Chunk>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Chunk {
    chunk: near_primitives::sharding::ShardChunk,
    processed_delayed_receipts: Vec<Receipt>,
    incoming_receipts: Vec<Receipt>,
    state: PartialState,
}

impl AdHocCommand {
    #[allow(dead_code)]
    pub fn run(&self, home: &Path) -> anyhow::Result<()> {
        match self.action.as_str() {
            "save" => self.save(home),
            "analyze" => self.wip(home),
            _ => {
                panic!("unknown cmd");
            }
        }
    }

    #[allow(dead_code)]
    fn wip(&self, _home: &Path) -> anyhow::Result<()> {
        let all_data = AllData::try_from_slice(&std::fs::read("/Users/pugachag/Data/neat_dump")?)?;
        let mut targets_overall = HashMap::<AccountId, usize>::new();
        let mut tot_kaiching_chunks = 0;
        let cache = TrieCache::new(
            &Default::default(),
            ShardLayout::get_simple_nightshade_layout().shard_uids().skip(2).next().unwrap(),
            false,
        );
        for block in all_data.blocks {
            let mut targets = HashMap::<AccountId, usize>::new();
            println!("===> height: {}", block.block.header().height());
            let Some(chunk) = &block.chunks.iter().filter(|c| c.chunk.shard_id() == 2).next()
            else {
                continue;
            };
            for rcp in &chunk.processed_delayed_receipts {
                *targets.entry(rcp.receiver_id.clone()).or_default() += 1;
                *targets_overall.entry(rcp.receiver_id.clone()).or_default() += 1;
            }
            if targets
                .get(&AccountId::from_str("earn.kaiching").unwrap())
                .cloned()
                .unwrap_or_default()
                >= 4
            {
                tot_kaiching_chunks += 1;
            }
            let PartialState::TrieValues(nodes) = &chunk.state;
            //assert_eq!(chunk.chunk.shard_id(), 0);
            let mut all_flat = HashSet::new();
            let mut misses = 0;
            let mut cache_updates = Vec::new();
            for node in nodes.iter() {
                let key = hash(node);
                if cache.get(&key).is_none() {
                    misses += 1;
                    cache_updates.push((key, Some(node.as_ref().to_vec())));
                }
                if RawTrieNodeWithSize::try_from_slice(node.as_ref()).is_err() {
                    all_flat.insert(hash(node.as_ref()));
                }
            }
            cache.update_cache(
                cache_updates
                    .iter()
                    .map(|pr| (&pr.0, pr.1.as_ref().map(|v| v.as_slice())))
                    .collect::<Vec<_>>(),
            );
            for node in nodes.iter() {
                if let Ok(node) = RawTrieNodeWithSize::try_from_slice(node.as_ref()) {
                    match node.node {
                        near_store::RawTrieNode::Leaf(_, value_ref) => {
                            all_flat.remove(&value_ref.hash);
                        }
                        near_store::RawTrieNode::BranchWithValue(value_ref, _) => {
                            all_flat.remove(&value_ref.hash);
                        }
                        _ => {}
                    }
                };
            }
            let trie = Trie::from_recorded_storage(
                PartialStorage { nodes: chunk.state.clone() },
                chunk.chunk.prev_state_root(),
                true,
            );
            let mut cnt = 0;
            let mut cols = vec![0; 10];
            for item in trie.iter().unwrap() {
                let Ok((key, _value)) = item else {
                    continue;
                };
                cols[key[0] as usize] += 1;
                cnt += 1;
            }
            for (i, s) in
                [NON_DELAYED_RECEIPT_COLUMNS.as_slice(), [(7, "DelayedReceipts")].as_slice()]
                    .concat()
            {
                println!("{s}: {}", cols[i as usize]);
            }
            let mut targets = targets.iter().collect::<Vec<_>>();
            targets.sort_by_key(|pr| std::cmp::Reverse(pr.1));
            println!("-> proc delayed receipts:");
            for (acc, cnt) in targets.iter().take(5) {
                println!("{acc}: {cnt}");
            }
            println!("kv: {cnt}, nodes: {}, shard cache misses: {misses} (cache size: {})), flat: {}", nodes.len(), cache.size() / 1000_000, all_flat.len());
        }

        println!(" ============ Overall:");

        let mut targets = targets_overall.iter().collect::<Vec<_>>();
        targets.sort_by_key(|pr| std::cmp::Reverse(pr.1));
        for (acc, cnt) in targets.iter().take(10) {
            println!("{acc}: {cnt}");
        }
        //let cnt_earn_kaiching = targets_overall.get(&AccountId::from_str("earn.kaiching").unwrap()).cloned().unwrap_or_default();
        println!("tot earn.kaiching chunks: {}", tot_kaiching_chunks);

        Ok(())
    }

    #[allow(dead_code)]
    fn analyze(&self, _home: &Path) -> anyhow::Result<()> {
        let all_data = AllData::try_from_slice(&std::fs::read("/Users/pugachag/Data/neat_dump")?)?;
        println!("blocks: {}", all_data.blocks.len());
        let mut trx_cnt = Vec::new();
        let mut inscribes = 0;
        let mut chunks2 = 0;
        let mut targets = HashMap::<AccountId, usize>::new();
        for block in &all_data.blocks {
            println!("block height {}", block.block.header().height());
            for chunk in &block.chunks {
                let PartialState::TrieValues(nodes) = &chunk.state;
                let mut sw = Vec::new();
                chunk.state.serialize(&mut sw).unwrap();
                println!(
                    "chunk {}: nodes {} (sw size: {}KB)",
                    chunk.chunk.shard_id(),
                    nodes.len(),
                    sw.len() / 1000
                );
                for rcp in &chunk.processed_delayed_receipts {
                    *targets.entry(rcp.receiver_id.clone()).or_default() += 1;
                }
                let chunk = &chunk.chunk;
                if chunk.shard_id() == 0 && chunk.height_included() == block.block.header().height()
                {
                    chunks2 += 1;
                }
                if chunk.shard_id() == 2 {
                    for trx in chunk.transactions() {
                        /*
                        if trx.transaction.receiver_id == AccountId::from_str("token.sweat").unwrap() {
                            println!("{:?}", trx);
                        }
                        */
                        for action in &trx.transaction.actions {
                            match action {
                                Action::FunctionCall(act) => {
                                    if act.method_name == "inscribe" {
                                        inscribes += 1;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    trx_cnt.push(chunk.prev_outgoing_receipts().len());
                }
            }
        }
        let mut targets = targets.iter().collect::<Vec<_>>();
        targets.sort_by_key(|pr| std::cmp::Reverse(pr.1));
        for (acc, cnt) in targets.iter().take(10) {
            println!("{acc}: {cnt}");
        }
        println!("missed chunks shard 0: {}", all_data.blocks.len() - chunks2);
        trx_cnt.sort();
        let n = trx_cnt.len();
        let tot: usize = trx_cnt.iter().sum();
        println!("total inscribe calls: {}, total transactions: {}", inscribes, tot);
        println!("trx avg: {}, p50 {}, p99 {}", tot / n, trx_cnt[n / 2], trx_cnt[n * 99 / 100]);
        Ok(())
    }

    #[allow(dead_code)]
    fn save(&self, home: &Path) -> anyhow::Result<()> {
        const FILE_PATH: &str = "/home/ubuntu/mock/.near/data/neat_dump";
        //const HEIGHT_START: u64 = 106_845_000;
        //const HEIGHT_START_GOOD: u64 = 106_882_000;
        const HEIGHT_START_BAD: u64 = 106_882_700;
        const HEIGHT_START: u64 = HEIGHT_START_BAD - 100;
        //const HEIGHT_END: u64 = 106_850_000;
        //const HEIGHT_END: u64 = 106_868_000;
        const HEIGHT_END: u64 = HEIGHT_START + 160;
        let rocksdb = Arc::new(open_rocksdb(home, near_store::Mode::ReadWrite)?);
        let store = near_store::NodeStorage::new(rocksdb.clone()).get_hot_store();
        let near_config = nearcore::config::load_config(
            &home,
            near_chain_configs::GenesisValidationMode::UnsafeFast,
        )?;
        let epoch_manager =
            EpochManager::new_arc_handle(store.clone(), &near_config.genesis.config);
        let runtime =
            NightshadeRuntime::from_config(&home, store.clone(), &near_config, epoch_manager);

        let mut blocks = Vec::new();
        let progress = ProgressBar::new(HEIGHT_END - HEIGHT_START);
        let mut height_skipped = 0;
        for height in HEIGHT_START..=HEIGHT_END {
            progress.inc(1);
            let Some(block_hash_bytes) =
                rocksdb.get_raw_bytes(DBCol::BlockHeight, &height.to_le_bytes()).unwrap()
            else {
                height_skipped += 1;
                continue;
            };
            let hash = CryptoHash::try_from(block_hash_bytes.as_ref()).unwrap();
            let Some(block_bytes) = rocksdb.get_raw_bytes(DBCol::Block, hash.as_bytes()).unwrap()
            else {
                continue;
            };
            let block =
                near_primitives::block::Block::try_from_slice(block_bytes.as_slice()).unwrap();
            let mut chunks = Vec::new();
            for chunk_header in block.chunks().iter() {
                let chunk_bytes = rocksdb
                    .get_raw_bytes(DBCol::Chunks, chunk_header.chunk_hash().as_bytes())
                    .unwrap()
                    .unwrap();
                let chunk =
                    near_primitives::sharding::ShardChunk::try_from_slice(chunk_bytes.as_slice())
                        .unwrap();
                let incoming_receipts_key = get_block_shard_id(block.hash(), chunk.shard_id());
                let receipt_proofs =
                    Vec::<near_primitives::sharding::ReceiptProof>::try_from_slice(
                        rocksdb
                            .get_raw_bytes(DBCol::IncomingReceipts, &incoming_receipts_key)
                            .unwrap()
                            .unwrap()
                            .as_slice(),
                    )
                    .unwrap();
                let incoming_receipts: Vec<Receipt> = receipt_proofs
                    .into_iter()
                    .flat_map(|near_primitives::sharding::ReceiptProof(receipts, _)| receipts)
                    .collect();

                let storage_config = RuntimeStorageConfig {
                    state_root: chunk.prev_state_root(),
                    use_flat_storage: true,
                    source: near_chain::types::StorageDataSource::DbTrieOnly,
                    state_patch: Default::default(),
                    record_storage: true,
                };
                let chunk_header = chunk.cloned_header();
                let chunk_context = ApplyTransactionsChunkContext {
                    shard_id: chunk.shard_id(),
                    last_validator_proposals: chunk_header.prev_validator_proposals(),
                    gas_limit: chunk_header.gas_limit(),
                    is_new_chunk: true,
                    is_first_block_with_chunk_of_version: false,
                };
                let block_context = ApplyTransactionsBlockContext::from_header(
                    block.header(),
                    block.header().next_gas_price(),
                );
                let Ok(apply_res) = runtime.apply_transactions(
                    storage_config,
                    chunk_context,
                    block_context,
                    &incoming_receipts,
                    chunk.transactions(),
                ) else {
                    continue;
                };
                chunks.push(Chunk {
                    chunk,
                    incoming_receipts,
                    state: apply_res.proof.unwrap().nodes,
                    processed_delayed_receipts: apply_res.processed_delayed_receipts,
                });
            }
            blocks.push(Block { block, chunks });
        }
        progress.finish();
        println!("total: {} blocks, skipped {}", blocks.len(), height_skipped);
        let mut bytes = Vec::new();
        AllData { blocks }.serialize(&mut bytes).unwrap();
        std::fs::write(FILE_PATH, &bytes)?;
        Ok(())
    }
}

use alloy_primitives::{hex, Log as LogStruct, B256};
use alloy_provider::{Provider, ReqwestProvider};
use alloy_rpc_types::{Filter, Header, Log, Transaction as AlloyRpcTransaction};
use alloy_sol_types::{SolCall, SolEvent};
use anyhow::{anyhow, bail, ensure, Result};
use kzg::kzg_types::ZFr;
use kzg_traits::{
    eip_4844::{blob_to_kzg_commitment_rust, Blob},
    Fr, G1,
};
use raiko_lib::{
    builder::{OptimisticDatabase, RethBlockBuilder},
    clear_line,
    consts::ChainSpec,
    inplace_print,
    input::{
        ontake::{BlockProposedV2, CalldataTxList},
        pacaya::{proposeBatchCall, BatchProposed},
        proposeBlockCall, BlobProofType, BlockProposed, BlockProposedFork, TaikoGuestBatchInput,
        TaikoGuestInput, TaikoProverData,
    },
    primitives::eip4844::{self, commitment_to_version_hash, KZG_SETTINGS},
};
use reth_evm_ethereum::taiko::{decode_anchor, decode_anchor_ontake, decode_anchor_pacaya};
use reth_primitives::{Block as RethBlock, TransactionSigned};
use reth_revm::primitives::SpecId;
use serde::{Deserialize, Serialize};
use std::iter;
use tracing::{debug, error, info, warn};

use crate::{
    interfaces::{RaikoError, RaikoResult},
    provider::{db::ProviderDb, rpc::RpcBlockDataProvider, BlockDataProvider},
    require,
};

/// Optimize data gathering by executing the transactions multiple times so data can be requested in batches
pub async fn execute_txs<'a, BDP>(
    builder: &mut RethBlockBuilder<ProviderDb<'a, BDP>>,
    pool_txs: Vec<reth_primitives::TransactionSigned>,
) -> RaikoResult<()>
where
    BDP: BlockDataProvider,
{
    let max_iterations = 100;
    for num_iterations in 0.. {
        inplace_print(&format!("Executing iteration {num_iterations}..."));

        let Some(db) = builder.db.as_mut() else {
            return Err(RaikoError::Preflight("No db in builder".to_owned()));
        };
        db.optimistic = num_iterations + 1 < max_iterations;

        builder
            .execute_transactions(pool_txs.clone(), num_iterations + 1 < max_iterations)
            .map_err(|e| {
                RaikoError::Preflight(format!("Executing transactions in builder failed: {e}"))
            })?;

        let Some(db) = builder.db.as_mut() else {
            return Err(RaikoError::Preflight("No db in builder".to_owned()));
        };
        if db.fetch_data().await {
            clear_line();
            info!("State data fetched in {num_iterations} iterations");
            break;
        }
    }

    Ok(())
}

/// Prepare the input for a Taiko chain
pub async fn prepare_taiko_chain_input(
    l1_chain_spec: &ChainSpec,
    taiko_chain_spec: &ChainSpec,
    block_number: u64,
    l1_inclusion_block_number: Option<u64>,
    block: &RethBlock,
    prover_data: TaikoProverData,
    blob_proof_type: &BlobProofType,
) -> RaikoResult<TaikoGuestInput> {
    // Decode the anchor tx to find out which L1 blocks we need to fetch
    let anchor_tx = block
        .body
        .first()
        .ok_or_else(|| RaikoError::Preflight("No anchor tx in the block".to_owned()))?;

    // get anchor block num and state root
    let fork = taiko_chain_spec.active_fork(block.number, block.timestamp)?;
    let (anchor_block_height, anchor_state_root) = match fork {
        SpecId::PACAYA => {
            warn!("pacaya fork does not support prepare_taiko_chain_input for single block");
            return Err(RaikoError::Preflight(
                "pacaya fork does not support prepare_taiko_chain_input for single block"
                    .to_owned(),
            ));
        }
        SpecId::ONTAKE => {
            let anchor_call = decode_anchor_ontake(anchor_tx.input())?;
            (anchor_call._anchorBlockId, anchor_call._anchorStateRoot)
        }
        _ => {
            let anchor_call = decode_anchor(anchor_tx.input())?;
            (anchor_call.l1BlockId, anchor_call.l1StateRoot)
        }
    };

    // // Get the L1 block in which the L2 block was included so we can fetch the DA data.
    // // Also get the L1 state block header so that we can prove the L1 state root.
    let provider_l1 = RpcBlockDataProvider::new(&l1_chain_spec.rpc, 0).await?;

    info!("current taiko chain fork: {fork:?}");

    let (l1_inclusion_block_number, proposal_tx, block_proposed) =
        if let Some(l1_block_number) = l1_inclusion_block_number {
            // Get the block proposal data
            get_block_proposed_event_by_height(
                provider_l1.provider(),
                taiko_chain_spec.clone(),
                l1_block_number,
                block_number,
                fork,
            )
            .await?
        } else {
            // traversal next 64 blocks to get proposal data
            get_block_proposed_event_by_traversal(
                provider_l1.provider(),
                taiko_chain_spec.clone(),
                anchor_block_height,
                block_number,
                fork,
            )
            .await?
        };

    let (l1_inclusion_header, l1_state_header) = get_headers(
        &provider_l1,
        (l1_inclusion_block_number, anchor_block_height),
    )
    .await?;
    assert_eq!(anchor_state_root, l1_state_header.state_root);
    let l1_state_block_hash = l1_state_header.hash.ok_or_else(|| {
        RaikoError::Preflight("No L1 state block hash for the requested block".to_owned())
    })?;
    let l1_inclusion_block_hash = l1_inclusion_header.hash.ok_or_else(|| {
        RaikoError::Preflight("No L1 inclusion block hash for the requested block".to_owned())
    })?;
    info!(
        "L1 inclusion block number: {l1_inclusion_block_number:?}, hash: {l1_inclusion_block_hash:?}. L1 state block number: {:?}, hash: {l1_state_block_hash:?}",
        l1_state_header.number,
    );

    // Fetch the tx data from either calldata or blobdata
    let (tx_data, blob_commitment, blob_proof) = if block_proposed.blob_used() {
        let expected_blob_hash = block_proposed.blob_hash();
        let blob_hashes = proposal_tx.blob_versioned_hashes.unwrap_or_default();
        // Get the blob hashes attached to the propose tx and make sure the expected blob hash is in there
        require(
            blob_hashes.contains(&expected_blob_hash),
            &format!(
                "Proposal blobs hash mismatch: {:?} not in {:?}",
                expected_blob_hash, blob_hashes
            ),
        )?;

        get_tx_blob(
            expected_blob_hash,
            l1_inclusion_header.timestamp,
            l1_chain_spec,
            blob_proof_type,
        )
        .await?
    } else {
        match fork {
            SpecId::PACAYA => {
                warn!("pacaya fork does not support prepare_taiko_chain_input for single block");
                return Err(RaikoError::Preflight(
                    "pacaya fork does not support prepare_taiko_chain_input for single block"
                        .to_owned(),
                ));
            }
            SpecId::ONTAKE => {
                // Get the tx list data directly from the propose block CalldataTxList event
                let (_, CalldataTxList { txList, .. }) = get_calldata_txlist_event(
                    provider_l1.provider(),
                    taiko_chain_spec.clone(),
                    l1_inclusion_block_hash,
                    block_number,
                )
                .await?;
                (txList.to_vec(), None, None)
            }
            _ => {
                // Get the tx list data directly from the propose transaction data
                let proposeBlockCall { txList, .. } =
                    proposeBlockCall::abi_decode(&proposal_tx.input, false).map_err(|_| {
                        RaikoError::Preflight("Could not decode proposeBlockCall".to_owned())
                    })?;
                (txList.to_vec(), None, None)
            }
        }
    };

    // Create the input struct without the block data set
    Ok(TaikoGuestInput {
        l1_header: l1_state_header.try_into().unwrap(),
        tx_data,
        anchor_tx: Some(anchor_tx.clone()),
        blob_commitment,
        block_proposed,
        prover_data,
        blob_proof,
        blob_proof_type: blob_proof_type.clone(),
    })
}

// get fork corresponding anchor block height and state root
fn get_anchor_tx_info_by_fork(
    fork: SpecId,
    anchor_tx: &TransactionSigned,
) -> RaikoResult<(u64, B256)> {
    match fork {
        SpecId::PACAYA => {
            let anchor_call = decode_anchor_pacaya(anchor_tx.input())?;
            Ok((anchor_call._anchorBlockId, anchor_call._anchorStateRoot))
        }
        SpecId::ONTAKE => {
            let anchor_call = decode_anchor_ontake(anchor_tx.input())?;
            Ok((anchor_call._anchorBlockId, anchor_call._anchorStateRoot))
        }
        _ => {
            let anchor_call = decode_anchor(anchor_tx.input())?;
            Ok((anchor_call.l1BlockId, anchor_call.l1StateRoot))
        }
    }
}

/// a problem here is that we need to know the fork of the batch proposal tx
/// but in batch mode, there is no block number in proof request
/// so we hard code the fork to pacaya here.
/// return the block numbers of the batch, i.e. [start(lastBlockId - len() + 1), end(lastBlockId)]
pub async fn parse_l1_batch_proposal_tx_for_pacaya_fork(
    l1_chain_spec: &ChainSpec,
    taiko_chain_spec: &ChainSpec,
    l1_inclusion_block_number: u64,
    batch_id: u64,
) -> RaikoResult<Vec<u64>> {
    let provider_l1 = RpcBlockDataProvider::new(&l1_chain_spec.rpc, 0).await?;
    let (l1_inclusion_height, _tx, batch_proposed_fork) = get_block_proposed_event_by_height(
        provider_l1.provider(),
        taiko_chain_spec.clone(),
        l1_inclusion_block_number,
        batch_id,
        SpecId::PACAYA,
    )
    .await?;

    assert!(
        l1_inclusion_block_number == l1_inclusion_height,
        "proposal tx inclusive block != proof_request block"
    );
    if let BlockProposedFork::Pacaya(batch_proposed) = batch_proposed_fork {
        let batch_info = &batch_proposed.info;
        Ok(
            ((batch_info.lastBlockId - (batch_info.blocks.len() as u64 - 1))
                ..=batch_info.lastBlockId)
                .collect(),
        )
    } else {
        Err(RaikoError::Preflight(
            "BatchProposedFork is not Pacaya".to_owned(),
        ))
    }
}

/// Prepare the input for a Taiko chain
pub async fn prepare_taiko_chain_batch_input(
    l1_chain_spec: &ChainSpec,
    taiko_chain_spec: &ChainSpec,
    l1_inclusion_block_number: u64,
    batch_id: u64,
    batch_blocks: &[RethBlock],
    prover_data: TaikoProverData,
    blob_proof_type: &BlobProofType,
) -> RaikoResult<TaikoGuestBatchInput> {
    // Get the L1 block in which the L2 block was included so we can fetch the DA data.
    // Also get the L1 state block header so that we can prove the L1 state root.
    // Decode the anchor tx to find out which L1 blocks we need to fetch
    let batch_anchor_tx_info = batch_blocks.iter().try_fold(Vec::new(), |mut acc, block| {
        let anchor_tx = block
            .body
            .first()
            .ok_or_else(|| RaikoError::Preflight("No anchor tx in the block".to_owned()))?;
        let fork = taiko_chain_spec.active_fork(block.number, block.timestamp)?;
        ensure!(fork == SpecId::PACAYA, "Only pacaya fork supports batch");
        let anchor_info = get_anchor_tx_info_by_fork(fork, anchor_tx)?;
        acc.push(anchor_info);
        Ok(acc)
    })?;

    assert!(
        batch_anchor_tx_info.windows(2).all(|w| w[0] == w[1]),
        "batch anchor tx info mismatch"
    );

    let (anchor_block_height, anchor_state_root) = batch_anchor_tx_info[0];
    let fork = taiko_chain_spec.active_fork(batch_blocks[0].number, batch_blocks[0].timestamp)?;
    let provider_l1 = RpcBlockDataProvider::new(&l1_chain_spec.rpc, 0).await?;
    // todo: duplicate code with parse_l1_batch_proposal_tx_for_pacaya_fork(), better to make these values fn parameters
    let (l1_inclusion_height, batch_proposal_tx, batch_proposed_fork) =
        get_block_proposed_event_by_height(
            provider_l1.provider(),
            taiko_chain_spec.clone(),
            l1_inclusion_block_number,
            batch_id,
            fork,
        )
        .await?;
    assert_eq!(l1_inclusion_block_number, l1_inclusion_height);
    let (l1_inclusion_header, l1_state_header) = get_headers(
        &provider_l1,
        (l1_inclusion_block_number, anchor_block_height),
    )
    .await?;
    assert_eq!(anchor_state_root, l1_state_header.state_root);

    if let BlockProposedFork::Pacaya(batch_proposed) = batch_proposed_fork {
        let batch_info = &batch_proposed.info;
        let blob_hashes = batch_info.blobHashes.clone();
        let force_inclusion_block_number = batch_info.blobCreatedIn;
        let l1_blob_timestamp = if force_inclusion_block_number != 0
            && force_inclusion_block_number != l1_inclusion_block_number
        {
            // force inclusion block
            info!(
                "process force inclusion block: {l1_inclusion_block_number:?} -> {force_inclusion_block_number:?}"
            );
            let (force_inclusion_header, _) = get_headers(
                &provider_l1,
                (force_inclusion_block_number, anchor_block_height),
            )
            .await?;
            force_inclusion_header.timestamp
        } else {
            l1_inclusion_header.timestamp
        };

        // according to protocol, calldata is mutex with blob
        let (tx_data_from_calldata, blob_tx_buffers_with_proofs) = if blob_hashes.is_empty() {
            let proposeBatchCall { _txList, .. } =
                proposeBatchCall::abi_decode(&batch_proposal_tx.input, false).map_err(|_| {
                    RaikoError::Preflight("Could not decode proposeBatchCall".to_owned())
                })?;
            (_txList.to_vec(), Vec::new())
        } else {
            let blob_tx_buffers = get_batch_tx_data_with_proofs(
                blob_hashes,
                l1_blob_timestamp,
                l1_chain_spec,
                blob_proof_type,
            )
            .await?;
            (Vec::new(), blob_tx_buffers)
        };

        return Ok(TaikoGuestBatchInput {
            batch_id: batch_id,
            batch_proposed: BlockProposedFork::Pacaya(batch_proposed),
            l1_header: l1_state_header.try_into().unwrap(),
            chain_spec: taiko_chain_spec.clone(),
            prover_data: prover_data,
            tx_data_from_calldata,
            tx_data_from_blob: blob_tx_buffers_with_proofs
                .iter()
                .map(|(blob_tx_data, _, _)| blob_tx_data.clone())
                .collect(),
            blob_commitments: blob_tx_buffers_with_proofs
                .iter()
                .map(|(_, commit, _)| commit.clone())
                .collect(),
            blob_proofs: blob_tx_buffers_with_proofs
                .iter()
                .map(|(_, _, proof)| proof.clone())
                .collect(),
            blob_proof_type: blob_proof_type.clone(),
        });
    } else {
        Err(RaikoError::Preflight(
            "BatchProposedFork is not Pacaya".to_owned(),
        ))
    }
}

pub async fn get_tx_blob(
    blob_hash: B256,
    timestamp: u64,
    chain_spec: &ChainSpec,
    blob_proof_type: &BlobProofType,
) -> RaikoResult<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)> {
    debug!("get tx from hash blob: {blob_hash:?}");
    // Get the blob data for this block
    let slot_id = block_time_to_block_slot(
        timestamp,
        chain_spec.genesis_time,
        chain_spec.seconds_per_slot,
    )?;
    let beacon_rpc_url: String = chain_spec.beacon_rpc.clone().ok_or_else(|| {
        RaikoError::Preflight("Beacon RPC URL is required for Taiko chains".to_owned())
    })?;
    let blob = get_and_filter_blob_data(&beacon_rpc_url, slot_id, blob_hash).await?;
    let commitment = eip4844::calc_kzg_proof_commitment(&blob).map_err(|e| anyhow!(e))?;
    let blob_proof = match blob_proof_type {
        BlobProofType::KzgVersionedHash => None,
        BlobProofType::ProofOfEquivalence => {
            let (x, y) =
                eip4844::proof_of_equivalence(&blob, &commitment_to_version_hash(&commitment))
                    .map_err(|e| anyhow!(e))?;

            debug!("x {x:?} y {y:?}");
            let point = eip4844::calc_kzg_proof_with_point(&blob, ZFr::from_bytes(&x).unwrap());
            debug!("calc_kzg_proof_with_point {point:?}");

            Some(
                point
                    .map(|g1| g1.to_bytes().to_vec())
                    .map_err(|e| anyhow!(e))?,
            )
        }
    };

    Ok((blob, Some(commitment.to_vec()), blob_proof))
}

pub async fn filter_tx_blob_beacon_with_proof(
    blob_hash: B256,
    blobs: Vec<String>,
    blob_proof_type: &BlobProofType,
) -> RaikoResult<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)> {
    info!("get tx from hash blob: {blob_hash:?}");
    // Get the blob data for this block
    let blob = filter_blob_data_beacon(blobs, blob_hash).await?;
    let commitment = eip4844::calc_kzg_proof_commitment(&blob).map_err(|e| anyhow!(e))?;
    let blob_proof = match blob_proof_type {
        BlobProofType::KzgVersionedHash => None,
        BlobProofType::ProofOfEquivalence => {
            let (x, y) =
                eip4844::proof_of_equivalence(&blob, &commitment_to_version_hash(&commitment))
                    .map_err(|e| anyhow!(e))?;

            debug!("x {x:?} y {y:?}");
            let point = eip4844::calc_kzg_proof_with_point(&blob, ZFr::from_bytes(&x).unwrap());
            debug!("calc_kzg_proof_with_point {point:?}");

            Some(
                point
                    .map(|g1| g1.to_bytes().to_vec())
                    .map_err(|e| anyhow!(e))?,
            )
        }
    };

    Ok((blob, Some(commitment.to_vec()), blob_proof))
}

/// get tx data(blob data) vec from blob hashes
/// and get proofs for each blobs
pub async fn get_batch_tx_data_with_proofs(
    blob_hashes: Vec<B256>,
    timestamp: u64,
    chain_spec: &ChainSpec,
    blob_proof_type: &BlobProofType,
) -> RaikoResult<Vec<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>> {
    let mut tx_data = Vec::new();
    let beacon_rpc_url: String = chain_spec.beacon_rpc.clone().ok_or_else(|| {
        RaikoError::Preflight("Beacon RPC URL is required for Taiko chains".to_owned())
    })?;
    let slot_id = block_time_to_block_slot(
        timestamp,
        chain_spec.genesis_time,
        chain_spec.seconds_per_slot,
    )?;
    // get blob data once
    let blob_data = get_blob_data(&beacon_rpc_url, slot_id).await?;
    let blobs: Vec<String> = blob_data.data.iter().map(|b| b.blob.clone()).collect();
    for hash in blob_hashes {
        let data = filter_tx_blob_beacon_with_proof(hash, blobs.clone(), blob_proof_type).await?;
        tx_data.push(data);
    }
    Ok(tx_data)
}

pub async fn filter_blockchain_event(
    provider: &ReqwestProvider,
    gen_block_event_filter: impl Fn() -> Filter,
) -> Result<Vec<Log>> {
    // Setup the filter to get the relevant events
    let filter = gen_block_event_filter();
    // Now fetch the events
    Ok(provider.get_logs(&filter).await?)
}

pub async fn get_calldata_txlist_event(
    provider: &ReqwestProvider,
    chain_spec: ChainSpec,
    block_hash: B256,
    l2_block_number: u64,
) -> Result<(AlloyRpcTransaction, CalldataTxList)> {
    // // Get the address that emitted the event
    let Some(l1_address) = chain_spec.l1_contract else {
        bail!("No L1 contract address in the chain spec");
    };

    let logs = filter_blockchain_event(provider, || {
        Filter::new()
            .address(l1_address)
            .at_block_hash(block_hash)
            .event_signature(CalldataTxList::SIGNATURE_HASH)
    })
    .await?;

    // Run over the logs returned to find the matching event for the specified L2 block number
    // (there can be multiple blocks proposed in the same block and even same tx)
    for log in logs {
        let Some(log_struct) = LogStruct::new(
            log.address(),
            log.topics().to_vec(),
            log.data().data.clone(),
        ) else {
            bail!("Could not create log")
        };
        let event = CalldataTxList::decode_log(&log_struct, false)
            .map_err(|_| RaikoError::Anyhow(anyhow!("Could not decode log")))?;
        if event.blockId == raiko_lib::primitives::U256::from(l2_block_number) {
            let Some(log_tx_hash) = log.transaction_hash else {
                bail!("No transaction hash in the log")
            };
            let tx = provider
                .get_transaction_by_hash(log_tx_hash)
                .await
                .expect("couldn't query the propose tx")
                .expect("Could not find the propose tx");
            return Ok((tx, event.data));
        }
    }
    bail!("No BlockProposedV2 event found for block {l2_block_number}");
}

pub enum EventFilterConditioin {
    #[allow(dead_code)]
    Hash(B256),
    Height(u64),
    Range((u64, u64)),
}

pub async fn filter_block_proposed_event(
    provider: &ReqwestProvider,
    chain_spec: ChainSpec,
    filter_condition: EventFilterConditioin,
    block_num_or_batch_id: u64,
    fork: SpecId,
) -> Result<(u64, AlloyRpcTransaction, BlockProposedFork)> {
    // Get the address that emitted the event
    let Some(l1_address) = chain_spec.l1_contract else {
        bail!("No L1 contract address in the chain spec");
    };

    // Get the event signature (value can differ between chains)
    let event_signature = match fork {
        SpecId::PACAYA => BatchProposed::SIGNATURE_HASH,
        SpecId::ONTAKE => BlockProposedV2::SIGNATURE_HASH,
        _ => BlockProposed::SIGNATURE_HASH,
    };
    // Setup the filter to get the relevant events
    let logs = filter_blockchain_event(provider, || match filter_condition {
        EventFilterConditioin::Hash(block_hash) => Filter::new()
            .address(l1_address)
            .at_block_hash(block_hash)
            .event_signature(event_signature),
        EventFilterConditioin::Height(block_number) => Filter::new()
            .address(l1_address)
            .from_block(block_number)
            .to_block(block_number + 1)
            .event_signature(event_signature),
        EventFilterConditioin::Range((from_block_number, to_block_number)) => Filter::new()
            .address(l1_address)
            .from_block(from_block_number)
            .to_block(to_block_number)
            .event_signature(event_signature),
    })
    .await?;

    // Run over the logs returned to find the matching event for the specified L2 block number
    // (there can be multiple blocks proposed in the same block and even same tx)
    for log in logs {
        let Some(log_struct) = LogStruct::new(
            log.address(),
            log.topics().to_vec(),
            log.data().data.clone(),
        ) else {
            bail!("Could not create log")
        };
        let (block_or_batch_id, block_propose_event) = match fork {
            SpecId::PACAYA => {
                let event = BatchProposed::decode_log(&log_struct, false)
                    .map_err(|_| RaikoError::Anyhow(anyhow!("Could not decode log")))?;
                (
                    raiko_lib::primitives::U256::from(event.meta.batchId),
                    BlockProposedFork::Pacaya(event.data),
                )
            }
            SpecId::ONTAKE => {
                let event = BlockProposedV2::decode_log(&log_struct, false)
                    .map_err(|_| RaikoError::Anyhow(anyhow!("Could not decode log")))?;
                (event.blockId, BlockProposedFork::Ontake(event.data))
            }
            _ => {
                let event = BlockProposed::decode_log(&log_struct, false)
                    .map_err(|_| RaikoError::Anyhow(anyhow!("Could not decode log")))?;
                (event.blockId, BlockProposedFork::Hekla(event.data))
            }
        };

        if block_or_batch_id == raiko_lib::primitives::U256::from(block_num_or_batch_id) {
            let Some(log_tx_hash) = log.transaction_hash else {
                bail!("No transaction hash in the log")
            };
            let tx = provider
                .get_transaction_by_hash(log_tx_hash)
                .await
                .expect("couldn't query the propose tx")
                .expect("Could not find the propose tx");
            return Ok((log.block_number.unwrap(), tx, block_propose_event));
        }
    }

    Err(anyhow!(
        "No BlockProposed event found for block {block_num_or_batch_id}"
    ))
}

pub async fn _get_block_proposed_event_by_hash(
    provider: &ReqwestProvider,
    chain_spec: ChainSpec,
    l1_inclusion_block_hash: B256,
    l2_block_number: u64,
    fork: SpecId,
) -> Result<(u64, AlloyRpcTransaction, BlockProposedFork)> {
    filter_block_proposed_event(
        provider,
        chain_spec,
        EventFilterConditioin::Hash(l1_inclusion_block_hash),
        l2_block_number,
        fork,
    )
    .await
}

pub async fn get_block_proposed_event_by_height(
    provider: &ReqwestProvider,
    chain_spec: ChainSpec,
    l1_inclusion_block_number: u64,
    block_num_or_batch_id: u64,
    fork: SpecId,
) -> Result<(u64, AlloyRpcTransaction, BlockProposedFork)> {
    filter_block_proposed_event(
        provider,
        chain_spec,
        EventFilterConditioin::Height(l1_inclusion_block_number),
        block_num_or_batch_id,
        fork,
    )
    .await
}

pub async fn get_block_proposed_event_by_traversal(
    provider: &ReqwestProvider,
    chain_spec: ChainSpec,
    l1_anchor_block_number: u64,
    l2_block_number: u64,
    fork: SpecId,
) -> Result<(u64, AlloyRpcTransaction, BlockProposedFork)> {
    let latest_block_number = provider.get_block_number().await?;
    let range_start = l1_anchor_block_number + 1;
    let range_end = std::cmp::min(l1_anchor_block_number + 64, latest_block_number);
    info!("traversal proposal event in L1 range: ({range_start}, {range_end})");
    filter_block_proposed_event(
        provider,
        chain_spec,
        EventFilterConditioin::Range((range_start, range_end)),
        l2_block_number,
        fork,
    )
    .await
}

pub async fn get_block_and_parent_data<BDP>(
    provider: &BDP,
    block_number: u64,
) -> RaikoResult<(RethBlock, alloy_rpc_types::Block)>
where
    BDP: BlockDataProvider,
{
    // Get the block and the parent block
    let blocks = provider
        .get_blocks(&[(block_number, true), (block_number - 1, false)])
        .await?;
    let mut blocks = blocks.iter();
    let Some(block) = blocks.next() else {
        return Err(RaikoError::Preflight(
            "No block data for the requested block".to_owned(),
        ));
    };
    let Some(parent_block) = blocks.next() else {
        return Err(RaikoError::Preflight(
            "No parent block data for the requested block".to_owned(),
        ));
    };

    info!(
        "Processing block {:?} with hash: {:?}",
        block.header.number,
        block.header.hash.unwrap(),
    );
    debug!("block.parent_hash: {:?}", block.header.parent_hash);
    debug!("block gas used: {:?}", block.header.gas_used);
    debug!("block transactions: {:?}", block.transactions.len());

    // Convert the alloy block to a reth block
    let block = RethBlock::try_from(block.clone())
        .map_err(|e| RaikoError::Conversion(format!("Failed converting to reth block: {e}")))?;
    Ok((block, parent_block.clone()))
}

pub async fn get_batch_blocks_and_parent_data<BDP>(
    provider: &BDP,
    block_numbers: &[u64],
) -> RaikoResult<Vec<(RethBlock, alloy_rpc_types::Block)>>
where
    BDP: BlockDataProvider,
{
    let target_blocks = iter::once(block_numbers[0] - 1)
        .chain(block_numbers.iter().cloned())
        .enumerate()
        .map(|(i, block_number)| (block_number, i != 0))
        .collect::<Vec<(u64, bool)>>();
    // Get the block and the parent block
    let blocks = provider.get_blocks(&target_blocks).await?;
    assert!(blocks.len() == block_numbers.len() + 1);

    info!(
        "Processing {} blocks with (num, hash) from:({:?}, {:?}) to ({:?}, {:?})",
        block_numbers.len(),
        blocks.first().unwrap().header.number,
        blocks.first().unwrap().header.hash.unwrap(),
        blocks.last().unwrap().header.number,
        blocks.last().unwrap().header.hash.unwrap(),
    );

    let pairs = blocks
        .windows(2)
        .map(|window_blocks| {
            let parent_block = &window_blocks[0];
            let prove_block = RethBlock::try_from(window_blocks[1].clone())
                .map_err(|e| {
                    RaikoError::Conversion(format!("Failed converting to reth block: {e}"))
                })
                .unwrap();
            (prove_block, parent_block.clone())
        })
        .collect();

    Ok(pairs)
}

pub async fn get_headers<BDP>(provider: &BDP, (a, b): (u64, u64)) -> RaikoResult<(Header, Header)>
where
    BDP: BlockDataProvider,
{
    // Get the block and the parent block
    let blocks = provider.get_blocks(&[(a, true), (b, false)]).await?;
    let mut blocks = blocks.iter();
    let Some(a) = blocks.next() else {
        return Err(RaikoError::Preflight(
            "No block data for the requested block".to_owned(),
        ));
    };
    let Some(b) = blocks.next() else {
        return Err(RaikoError::Preflight(
            "No block data for the requested block".to_owned(),
        ));
    };

    // Convert the alloy block to a reth block
    Ok((a.header.clone(), b.header.clone()))
}

// block_time_to_block_slot returns the slots of the given timestamp.
pub fn block_time_to_block_slot(
    block_time: u64,
    genesis_time: u64,
    block_per_slot: u64,
) -> RaikoResult<u64> {
    if genesis_time == 0 {
        Err(RaikoError::Anyhow(anyhow!(
            "genesis time is 0, please check chain spec"
        )))
    } else if block_time < genesis_time {
        Err(RaikoError::Anyhow(anyhow!(
            "provided block_time precedes genesis time",
        )))
    } else {
        Ok((block_time - genesis_time) / block_per_slot)
    }
}

pub fn blob_to_bytes(blob_str: &str) -> Vec<u8> {
    hex::decode(blob_str.to_lowercase().trim_start_matches("0x")).unwrap_or_default()
}

fn calc_blob_versioned_hash(blob_str: &str) -> [u8; 32] {
    let blob_bytes = hex::decode(blob_str.to_lowercase().trim_start_matches("0x"))
        .expect("Could not decode blob");
    let blob = Blob::from_bytes(&blob_bytes).expect("Could not create blob");
    let commitment = blob_to_kzg_commitment_rust(
        &eip4844::deserialize_blob_rust(&blob).expect("Could not deserialize blob"),
        &KZG_SETTINGS.clone(),
    )
    .expect("Could not create kzg commitment from blob");
    commitment_to_version_hash(&commitment.to_bytes()).0
}

async fn get_and_filter_blob_data(
    beacon_rpc_url: &str,
    block_id: u64,
    blob_hash: B256,
) -> Result<Vec<u8>> {
    if beacon_rpc_url.contains("blobscan.com") {
        get_and_filter_blob_data_by_blobscan(beacon_rpc_url, block_id, blob_hash).await
    } else {
        get_and_filter_blob_data_beacon(beacon_rpc_url, block_id, blob_hash).await
    }
}

async fn get_blob_data(beacon_rpc_url: &str, block_id: u64) -> Result<GetBlobsResponse> {
    if beacon_rpc_url.contains("blobscan.com") {
        unimplemented!("blobscan.com is not supported yet")
    } else {
        get_blob_data_beacon(beacon_rpc_url, block_id).await
    }
}

// Blob data from the beacon chain
// type Sidecar struct {
// Index                    string                   `json:"index"`
// Blob                     string                   `json:"blob"`
// SignedBeaconBlockHeader  *SignedBeaconBlockHeader `json:"signed_block_header"`
// KzgCommitment            string                   `json:"kzg_commitment"`
// KzgProof                 string                   `json:"kzg_proof"`
// CommitmentInclusionProof []string
// `json:"kzg_commitment_inclusion_proof"` }
#[derive(Clone, Debug, Deserialize, Serialize)]
struct GetBlobData {
    pub index: String,
    pub blob: String,
    // pub signed_block_header: SignedBeaconBlockHeader, // ignore for now
    pub kzg_commitment: String,
    pub kzg_proof: String,
    //pub kzg_commitment_inclusion_proof: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GetBlobsResponse {
    pub data: Vec<GetBlobData>,
}

async fn get_blob_data_beacon(beacon_rpc_url: &str, block_id: u64) -> Result<GetBlobsResponse> {
    let url = format!(
        "{}/eth/v1/beacon/blob_sidecars/{block_id}",
        beacon_rpc_url.trim_end_matches('/'),
    );
    info!("Retrieve blob from {url}.");
    let response = reqwest::get(url.clone()).await?;

    if !response.status().is_success() {
        warn!(
            "Request {url} failed with status code: {}",
            response.status()
        );
        return Err(anyhow::anyhow!(
            "Request failed with status code: {}",
            response.status()
        ));
    }

    let blobs = response.json::<GetBlobsResponse>().await?;
    ensure!(!blobs.data.is_empty(), "blob data not available anymore");
    Ok(blobs)
}

async fn get_and_filter_blob_data_beacon(
    beacon_rpc_url: &str,
    block_id: u64,
    blob_hash: B256,
) -> Result<Vec<u8>> {
    info!("Retrieve blob for {block_id} and expect {blob_hash}.");
    let blobs = get_blob_data_beacon(beacon_rpc_url, block_id).await?;
    // Get the blob data for the blob storing the tx list
    let tx_blob = blobs
        .data
        .iter()
        .find(|blob| {
            // calculate from plain blob
            blob_hash == calc_blob_versioned_hash(&blob.blob)
        })
        .cloned();

    if let Some(tx_blob) = &tx_blob {
        Ok(blob_to_bytes(&tx_blob.blob))
    } else {
        Err(anyhow!("couldn't find blob data matching blob hash"))
    }
}

async fn filter_blob_data_beacon(blobs: Vec<String>, blob_hash: B256) -> Result<Vec<u8>> {
    // Get the blob data for the blob storing the tx list
    let tx_blob = blobs
        .iter()
        .find(|blob| {
            // calculate from plain blob
            blob_hash == calc_blob_versioned_hash(blob)
        })
        .cloned();

    if let Some(tx_blob) = &tx_blob {
        Ok(blob_to_bytes(tx_blob))
    } else {
        Err(anyhow!("couldn't find blob data matching blob hash"))
    }
}

// https://api.blobscan.com/#/
#[derive(Clone, Debug, Deserialize, Serialize)]
struct BlobScanData {
    pub commitment: String,
    pub data: String,
}

async fn get_and_filter_blob_data_by_blobscan(
    beacon_rpc_url: &str,
    _block_id: u64,
    blob_hash: B256,
) -> Result<Vec<u8>> {
    let url = format!("{}/blobs/{blob_hash}", beacon_rpc_url.trim_end_matches('/'),);
    let response = reqwest::get(url.clone()).await?;

    if !response.status().is_success() {
        error!(
            "Request {url} failed with status code: {}",
            response.status()
        );
        return Err(anyhow::anyhow!(
            "Request failed with status code: {}",
            response.status()
        ));
    }

    let blob = response.json::<BlobScanData>().await?;
    Ok(blob_to_bytes(&blob.data))
}

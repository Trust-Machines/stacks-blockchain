// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use clarity::vm::costs::ExecutionCost;
use clarity::vm::types::PrincipalData;
use lazy_static::lazy_static;
use stacks::burnchains::MagicBytes;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::nakamoto::NakamotoChainState;
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::core::{
    StacksEpoch, StacksEpochId, BLOCK_LIMIT_MAINNET_10, HELIUM_BLOCK_LIMIT_20,
    PEER_VERSION_EPOCH_1_0, PEER_VERSION_EPOCH_2_0, PEER_VERSION_EPOCH_2_05,
    PEER_VERSION_EPOCH_2_1, PEER_VERSION_EPOCH_2_2, PEER_VERSION_EPOCH_2_3, PEER_VERSION_EPOCH_2_4,
    PEER_VERSION_EPOCH_2_5, PEER_VERSION_EPOCH_3_0,
};
use stacks_common::address::AddressHashMode;
use stacks_common::consts::STACKS_EPOCH_MAX;
use stacks_common::types::chainstate::{StacksAddress, StacksPublicKey};
use stacks_common::util::hash::to_hex;
use stacks_common::util::secp256k1::Secp256k1PrivateKey;

use super::bitcoin_regtest::BitcoinCoreController;
use crate::config::{EventKeyType, EventObserverConfig};
use crate::mockamoto::signer::SelfSigner;
use crate::neon::{Counters, RunLoopCounter};
use crate::run_loop::boot_nakamoto;
use crate::tests::make_stacks_transfer;
use crate::tests::neon_integrations::{
    get_account, get_pox_info, next_block_and_wait, run_until_burnchain_height, submit_tx,
    test_observer, wait_for_runloop,
};
use crate::{tests, BitcoinRegtestController, BurnchainController, Config, ConfigFile, Keychain};

static POX_4_DEFAULT_STACKER_BALANCE: u64 = 100_000_000_000_000;
static POX_4_DEFAULT_STACKER_STX_AMT: u128 = 99_000_000_000_000;

lazy_static! {
    pub static ref NAKAMOTO_INTEGRATION_EPOCHS: [StacksEpoch; 9] = [
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch10,
            start_height: 0,
            end_height: 0,
            block_limit: BLOCK_LIMIT_MAINNET_10.clone(),
            network_epoch: PEER_VERSION_EPOCH_1_0
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch20,
            start_height: 0,
            end_height: 1,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_0
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch2_05,
            start_height: 1,
            end_height: 2,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_05
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch21,
            start_height: 2,
            end_height: 3,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_1
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch22,
            start_height: 3,
            end_height: 4,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_2
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch23,
            start_height: 4,
            end_height: 5,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_3
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch24,
            start_height: 5,
            end_height: 201,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_4
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch25,
            start_height: 201,
            end_height: 231,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_5
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch30,
            start_height: 231,
            end_height: STACKS_EPOCH_MAX,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_3_0
        },
    ];
}

/// Return a working nakamoto-neon config and the miner's bitcoin address to fund
pub fn naka_neon_integration_conf(seed: Option<&[u8]>) -> (Config, StacksAddress) {
    let mut conf = super::new_test_conf();
    conf.burnchain.mode = "nakamoto-neon".into();

    // tests can override this, but these tests run with epoch 2.05 by default
    conf.burnchain.epochs = Some(NAKAMOTO_INTEGRATION_EPOCHS.to_vec());

    if let Some(seed) = seed {
        conf.node.seed = seed.to_vec();
    }

    // instantiate the keychain so we can fund the bitcoin op signer
    let keychain = Keychain::default(conf.node.seed.clone());

    let mining_key = Secp256k1PrivateKey::from_seed(&[1]);
    conf.miner.mining_key = Some(mining_key);
    conf.miner.self_signing_key = Some(SelfSigner::single_signer());

    conf.node.miner = true;
    conf.node.wait_time_for_microblocks = 500;
    conf.burnchain.burn_fee_cap = 20000;

    conf.burnchain.username = Some("neon-tester".into());
    conf.burnchain.password = Some("neon-tester-pass".into());
    conf.burnchain.peer_host = "127.0.0.1".into();
    conf.burnchain.local_mining_public_key =
        Some(keychain.generate_op_signer().get_public_key().to_hex());
    conf.burnchain.commit_anchor_block_within = 0;

    // test to make sure config file parsing is correct
    let mut cfile = ConfigFile::xenon();
    cfile.node.as_mut().map(|node| node.bootstrap_node.take());

    if let Some(burnchain) = cfile.burnchain.as_mut() {
        burnchain.peer_host = Some("127.0.0.1".to_string());
    }

    conf.burnchain.magic_bytes = MagicBytes::from(['T' as u8, '3' as u8].as_ref());
    conf.burnchain.poll_time_secs = 1;
    conf.node.pox_sync_sample_secs = 0;

    conf.miner.min_tx_fee = 1;
    conf.miner.first_attempt_time_ms = i64::max_value() as u64;
    conf.miner.subsequent_attempt_time_ms = i64::max_value() as u64;

    // if there's just one node, then this must be true for tests to pass
    conf.miner.wait_for_block_download = false;

    conf.node.mine_microblocks = false;
    conf.miner.microblock_attempt_time_ms = 10;
    conf.node.microblock_frequency = 0;
    conf.node.wait_time_for_blocks = 200;

    let miner_account = keychain.origin_address(conf.is_mainnet()).unwrap();

    conf.burnchain.pox_prepare_length = Some(5);
    conf.burnchain.pox_reward_length = Some(20);

    (conf, miner_account)
}

pub fn next_block_and<F>(
    btc_controller: &mut BitcoinRegtestController,
    timeout_secs: u64,
    mut check: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<bool, String>,
{
    eprintln!("Issuing bitcoin block");
    btc_controller.build_next_block(1);
    let start = Instant::now();
    while !check()? {
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            error!("Timed out waiting for block to process, trying to continue test");
            return Err("Timed out".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Mine a bitcoin block, and wait until:
///  (1) a new block has been processed by the coordinator
fn next_block_and_process_new_stacks_block(
    btc_controller: &mut BitcoinRegtestController,
    timeout_secs: u64,
    coord_channels: &Arc<Mutex<CoordinatorChannels>>,
) -> Result<(), String> {
    let blocks_processed_before = coord_channels
        .lock()
        .expect("Mutex poisoned")
        .get_stacks_blocks_processed();
    next_block_and(btc_controller, timeout_secs, || {
        let blocks_processed = coord_channels
            .lock()
            .expect("Mutex poisoned")
            .get_stacks_blocks_processed();
        if blocks_processed > blocks_processed_before {
            return Ok(true);
        }
        Ok(false)
    })
}

/// Mine a bitcoin block, and wait until:
///  (1) a new block has been processed by the coordinator
///  (2) 2 block commits have been issued ** or ** more than 10 seconds have
///      passed since (1) occurred
fn next_block_and_mine_commit(
    btc_controller: &mut BitcoinRegtestController,
    timeout_secs: u64,
    coord_channels: &Arc<Mutex<CoordinatorChannels>>,
    commits_submitted: &Arc<AtomicU64>,
) -> Result<(), String> {
    let commits_submitted = commits_submitted.clone();
    let blocks_processed_before = coord_channels
        .lock()
        .expect("Mutex poisoned")
        .get_stacks_blocks_processed();
    let commits_before = commits_submitted.load(Ordering::SeqCst);
    let mut block_processed_time: Option<Instant> = None;
    let mut commit_sent_time: Option<Instant> = None;
    next_block_and(btc_controller, timeout_secs, || {
        let commits_sent = commits_submitted.load(Ordering::SeqCst);
        let blocks_processed = coord_channels
            .lock()
            .expect("Mutex poisoned")
            .get_stacks_blocks_processed();
        let now = Instant::now();
        if blocks_processed > blocks_processed_before && block_processed_time.is_none() {
            block_processed_time.replace(now);
        }
        if commits_sent > commits_before && commit_sent_time.is_none() {
            commit_sent_time.replace(now);
        }
        if blocks_processed > blocks_processed_before {
            let block_processed_time = block_processed_time
                .as_ref()
                .ok_or("TEST-ERROR: Processed time wasn't set")?;
            if commits_sent <= commits_before {
                return Ok(false);
            }
            let commit_sent_time = commit_sent_time
                .as_ref()
                .ok_or("TEST-ERROR: Processed time wasn't set")?;
            // try to ensure the commit was sent after the block was processed
            if commit_sent_time > block_processed_time {
                return Ok(true);
            }
            // if two commits have been sent, one of them must have been after
            if commits_sent >= commits_before + 2 {
                return Ok(true);
            }
            // otherwise, just timeout if the commit was sent and its been long enough
            //  for a new commit pass to have occurred
            if block_processed_time.elapsed() > Duration::from_secs(10) {
                return Ok(true);
            }
            Ok(false)
        } else {
            Ok(false)
        }
    })
}

fn setup_stacker(naka_conf: &mut Config) -> Secp256k1PrivateKey {
    let stacker_sk = Secp256k1PrivateKey::new();
    let stacker_address = tests::to_addr(&stacker_sk);
    naka_conf.add_initial_balance(
        PrincipalData::from(stacker_address.clone()).to_string(),
        POX_4_DEFAULT_STACKER_BALANCE,
    );
    stacker_sk
}

///
/// * `stacker_sk` - must be a private key for sending a large `stack-stx` transaction in order
///   for pox-4 to activate
fn boot_to_epoch_3(
    naka_conf: &Config,
    blocks_processed: &RunLoopCounter,
    stacker_sk: Secp256k1PrivateKey,
    signer_pk: StacksPublicKey,
    btc_regtest_controller: &mut BitcoinRegtestController,
) {
    let epochs = naka_conf.burnchain.epochs.clone().unwrap();
    let epoch_3 = &epochs[StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch30).unwrap()];

    info!(
        "Chain bootstrapped to bitcoin block 201, starting Epoch 2x miner";
        "Epoch 3.0 Boundary" => (epoch_3.start_height - 1),
    );
    let http_origin = format!("http://{}", &naka_conf.node.rpc_bind);
    next_block_and_wait(btc_regtest_controller, &blocks_processed);
    next_block_and_wait(btc_regtest_controller, &blocks_processed);
    // first mined stacks block
    next_block_and_wait(btc_regtest_controller, &blocks_processed);

    // stack enough to activate pox-4
    let pox_addr_tuple = clarity::vm::tests::execute(&format!(
        "{{ hashbytes: 0x{}, version: 0x{:02x} }}",
        to_hex(&[0; 20]),
        AddressHashMode::SerializeP2PKH as u8,
    ));

    let stacking_tx = tests::make_contract_call(
        &stacker_sk,
        0,
        1000,
        &StacksAddress::burn_address(false),
        "pox-4",
        "stack-stx",
        &[
            clarity::vm::Value::UInt(POX_4_DEFAULT_STACKER_STX_AMT),
            pox_addr_tuple,
            clarity::vm::Value::UInt(205),
            clarity::vm::Value::UInt(12),
            clarity::vm::Value::buff_from(signer_pk.to_bytes_compressed()).unwrap(),
        ],
    );

    submit_tx(&http_origin, &stacking_tx);

    run_until_burnchain_height(
        btc_regtest_controller,
        &blocks_processed,
        epoch_3.start_height - 1,
        &naka_conf,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, Epoch2x miner should stop");
}

#[test]
#[ignore]
/// This test spins up a nakamoto-neon node.
/// It starts in Epoch 2.0, mines with `neon_node` to Epoch 3.0, and then switches
///  to Nakamoto operation (activating pox-4 by submitting a stack-stx tx). The BootLoop
///  struct handles the epoch-2/3 tear-down and spin-up.
/// This test makes three assertions:
///  * 30 blocks are mined after 3.0 starts. This is enough to mine across 2 reward cycles
///  * A transaction submitted to the mempool in 3.0 will be mined in 3.0
///  * The final chain tip is a nakamoto block
fn simple_neon_integration() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let (mut naka_conf, _miner_account) = naka_neon_integration_conf(None);
    naka_conf.miner.wait_on_interim_blocks = Duration::from_secs(1000);
    let sender_sk = Secp256k1PrivateKey::new();
    // setup sender + recipient for a test stx transfer
    let sender_addr = tests::to_addr(&sender_sk);
    let sender_signer_key = StacksPublicKey::new();
    let send_amt = 1000;
    let send_fee = 100;
    naka_conf.add_initial_balance(
        PrincipalData::from(sender_addr.clone()).to_string(),
        send_amt + send_fee,
    );
    let recipient = PrincipalData::from(StacksAddress::burn_address(false));
    let stacker_sk = setup_stacker(&mut naka_conf);

    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .expect("Failed starting bitcoind");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);
    btc_regtest_controller.bootstrap_chain(201);

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();

    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));
    wait_for_runloop(&blocks_processed);
    boot_to_epoch_3(
        &naka_conf,
        &blocks_processed,
        stacker_sk,
        sender_signer_key,
        &mut btc_regtest_controller,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, starting nakamoto miner");

    let burnchain = naka_conf.get_burnchain();
    let sortdb = burnchain.open_sortition_db(true).unwrap();
    let (mut chainstate, _) = StacksChainState::open(
        naka_conf.is_mainnet(),
        naka_conf.burnchain.chain_id,
        &naka_conf.get_chainstate_path_str(),
        None,
    )
    .unwrap();

    let block_height_pre_3_0 =
        NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
            .unwrap()
            .unwrap()
            .stacks_block_height;

    info!("Nakamoto miner started...");
    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(&mut btc_regtest_controller, 60, || {
        let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
        Ok(vrf_count >= 1)
    })
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(&mut btc_regtest_controller, 60, || {
        let commits_count = commits_submitted.load(Ordering::SeqCst);
        Ok(commits_count >= 1)
    })
    .unwrap();

    // Mine 15 nakamoto tenures
    for _i in 0..15 {
        next_block_and_mine_commit(
            &mut btc_regtest_controller,
            60,
            &coord_channel,
            &commits_submitted,
        )
        .unwrap();
    }

    // Submit a TX
    let transfer_tx = make_stacks_transfer(&sender_sk, 0, send_fee, &recipient, send_amt);
    let transfer_tx_hex = format!("0x{}", to_hex(&transfer_tx));

    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();

    let mut mempool = naka_conf
        .connect_mempool_db()
        .expect("Database failure opening mempool");

    mempool
        .submit_raw(
            &mut chainstate,
            &sortdb,
            &tip.consensus_hash,
            &tip.anchored_header.block_hash(),
            transfer_tx.clone(),
            &ExecutionCost::max_value(),
            &StacksEpochId::Epoch30,
        )
        .unwrap();

    // Mine 15 more nakamoto tenures
    for _i in 0..15 {
        next_block_and_mine_commit(
            &mut btc_regtest_controller,
            60,
            &coord_channel,
            &commits_submitted,
        )
        .unwrap();
    }

    // load the chain tip, and assert that it is a nakamoto block and at least 30 blocks have advanced in epoch 3
    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();
    info!(
        "Latest tip";
        "height" => tip.stacks_block_height,
        "is_nakamoto" => tip.anchored_header.as_stacks_nakamoto().is_some(),
    );

    // assert that the transfer tx was observed
    let transfer_tx_included = test_observer::get_blocks()
        .into_iter()
        .find(|block_json| {
            block_json["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tx_json| tx_json["raw_tx"].as_str() == Some(&transfer_tx_hex))
                .is_some()
        })
        .is_some();

    assert!(
        transfer_tx_included,
        "Nakamoto node failed to include the transfer tx"
    );

    assert!(tip.anchored_header.as_stacks_nakamoto().is_some());
    assert!(tip.stacks_block_height >= block_height_pre_3_0 + 30);

    coord_channel
        .lock()
        .expect("Mutex poisoned")
        .stop_chains_coordinator();
    run_loop_stopper.store(false, Ordering::SeqCst);

    run_loop_thread.join().unwrap();
}

#[test]
#[ignore]
/// This test spins up a nakamoto-neon node.
/// It starts in Epoch 2.0, mines with `neon_node` to Epoch 3.0, and then switches
///  to Nakamoto operation (activating pox-4 by submitting a stack-stx tx). The BootLoop
///  struct handles the epoch-2/3 tear-down and spin-up.
/// This test makes three assertions:
///  * 5 tenures are mined after 3.0 starts
///  * Each tenure has 10 blocks (the coinbase block and 9 interim blocks)
fn mine_multiple_per_tenure_integration() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let (mut naka_conf, _miner_account) = naka_neon_integration_conf(None);
    let http_origin = format!("http://{}", &naka_conf.node.rpc_bind);
    naka_conf.miner.wait_on_interim_blocks = Duration::from_secs(1);
    let sender_sk = Secp256k1PrivateKey::new();
    let sender_signer_key = StacksPublicKey::new();
    let tenure_count = 5;
    let inter_blocks_per_tenure = 9;
    // setup sender + recipient for some test stx transfers
    // these are necessary for the interim blocks to get mined at all
    let sender_addr = tests::to_addr(&sender_sk);
    let send_amt = 100;
    let send_fee = 180;
    naka_conf.add_initial_balance(
        PrincipalData::from(sender_addr.clone()).to_string(),
        (send_amt + send_fee) * tenure_count * inter_blocks_per_tenure,
    );
    let recipient = PrincipalData::from(StacksAddress::burn_address(false));
    let stacker_sk = setup_stacker(&mut naka_conf);

    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .expect("Failed starting bitcoind");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);
    btc_regtest_controller.bootstrap_chain(201);

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();

    let run_loop_thread = thread::Builder::new()
        .name("run_loop".into())
        .spawn(move || run_loop.start(None, 0))
        .unwrap();
    wait_for_runloop(&blocks_processed);
    boot_to_epoch_3(
        &naka_conf,
        &blocks_processed,
        stacker_sk,
        sender_signer_key,
        &mut btc_regtest_controller,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, starting nakamoto miner");

    let burnchain = naka_conf.get_burnchain();
    let sortdb = burnchain.open_sortition_db(true).unwrap();
    let (chainstate, _) = StacksChainState::open(
        naka_conf.is_mainnet(),
        naka_conf.burnchain.chain_id,
        &naka_conf.get_chainstate_path_str(),
        None,
    )
    .unwrap();

    let block_height_pre_3_0 =
        NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
            .unwrap()
            .unwrap()
            .stacks_block_height;

    info!("Nakamoto miner started...");
    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(&mut btc_regtest_controller, 60, || {
        let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
        Ok(vrf_count >= 1)
    })
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(&mut btc_regtest_controller, 60, || {
        let commits_count = commits_submitted.load(Ordering::SeqCst);
        Ok(commits_count >= 1)
    })
    .unwrap();

    // Mine `tenure_count` nakamoto tenures
    for tenure_ix in 0..tenure_count {
        let commits_before = commits_submitted.load(Ordering::SeqCst);
        next_block_and_process_new_stacks_block(&mut btc_regtest_controller, 60, &coord_channel)
            .unwrap();

        // mine the interim blocks
        for interim_block_ix in 0..inter_blocks_per_tenure {
            let blocks_processed_before = coord_channel
                .lock()
                .expect("Mutex poisoned")
                .get_stacks_blocks_processed();
            // submit a tx so that the miner will mine an extra block
            let sender_nonce = tenure_ix * inter_blocks_per_tenure + interim_block_ix;
            let transfer_tx =
                make_stacks_transfer(&sender_sk, sender_nonce, send_fee, &recipient, send_amt);
            submit_tx(&http_origin, &transfer_tx);

            loop {
                let blocks_processed = coord_channel
                    .lock()
                    .expect("Mutex poisoned")
                    .get_stacks_blocks_processed();
                if blocks_processed > blocks_processed_before {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        let start_time = Instant::now();
        while commits_submitted.load(Ordering::SeqCst) <= commits_before {
            if start_time.elapsed() >= Duration::from_secs(20) {
                panic!("Timed out waiting for block-commit");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    // load the chain tip, and assert that it is a nakamoto block and at least 30 blocks have advanced in epoch 3
    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();
    info!(
        "Latest tip";
        "height" => tip.stacks_block_height,
        "is_nakamoto" => tip.anchored_header.as_stacks_nakamoto().is_some(),
    );

    assert!(tip.anchored_header.as_stacks_nakamoto().is_some());
    assert_eq!(
        tip.stacks_block_height,
        block_height_pre_3_0 + ((inter_blocks_per_tenure + 1) * tenure_count),
        "Should have mined (1 + interim_blocks_per_tenure) * tenure_count nakamoto blocks"
    );

    coord_channel
        .lock()
        .expect("Mutex poisoned")
        .stop_chains_coordinator();
    run_loop_stopper.store(false, Ordering::SeqCst);

    run_loop_thread.join().unwrap();
}

#[test]
#[ignore]
fn correct_burn_outs() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let (mut naka_conf, _miner_account) = naka_neon_integration_conf(None);
    naka_conf.burnchain.pox_reward_length = Some(10);
    naka_conf.burnchain.pox_prepare_length = Some(3);

    {
        let epochs = naka_conf.burnchain.epochs.as_mut().unwrap();
        let epoch_24_ix = StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch24).unwrap();
        let epoch_25_ix = StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch25).unwrap();
        let epoch_30_ix = StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch30).unwrap();
        epochs[epoch_24_ix].end_height = 208;
        epochs[epoch_25_ix].start_height = 208;
        epochs[epoch_25_ix].end_height = 225;
        epochs[epoch_30_ix].start_height = 225;
    }

    naka_conf.miner.wait_on_interim_blocks = Duration::from_secs(1000);
    naka_conf.initial_balances.clear();
    let accounts: Vec<_> = (0..8)
        .map(|ix| {
            let sk = Secp256k1PrivateKey::from_seed(&[ix, ix, ix, ix]);
            let address = PrincipalData::from(tests::to_addr(&sk));
            (sk, address)
        })
        .collect();
    for (_, ref addr) in accounts.iter() {
        naka_conf.add_initial_balance(addr.to_string(), 10000000000000000);
    }

    let stacker_accounts = accounts[0..3].to_vec();

    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .expect("Failed starting bitcoind");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);
    btc_regtest_controller.bootstrap_chain(201);

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();

    let run_loop_thread = thread::Builder::new()
        .name("run_loop".into())
        .spawn(move || run_loop.start(None, 0))
        .unwrap();
    wait_for_runloop(&blocks_processed);

    let epochs = naka_conf.burnchain.epochs.clone().unwrap();
    let epoch_3 = &epochs[StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch30).unwrap()];
    let epoch_25 = &epochs[StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch25).unwrap()];

    info!(
        "Chain bootstrapped to bitcoin block 201, starting Epoch 2x miner";
        "Epoch 3.0 Boundary" => (epoch_3.start_height - 1),
    );

    run_until_burnchain_height(
        &mut btc_regtest_controller,
        &blocks_processed,
        epoch_25.start_height + 1,
        &naka_conf,
    );

    info!("Chain bootstrapped to Epoch 2.5, submitting stacker transaction");

    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let http_origin = format!("http://{}", &naka_conf.node.rpc_bind);
    let _stacker_thread = thread::Builder::new()
        .name("stacker".into())
        .spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            debug!("Checking for stacker-necessity");
            let Some(pox_info) = get_pox_info(&http_origin) else {
                warn!("Failed to get pox_info, waiting.");
                continue;
            };
            if !pox_info.contract_id.ends_with(".pox-4") {
                continue;
            }
            let next_cycle_stx = pox_info.next_cycle.stacked_ustx;
            let min_stx = pox_info.next_cycle.min_threshold_ustx;
            let min_stx = (min_stx * 3) / 2;
            if next_cycle_stx >= min_stx {
                debug!(
                    "Next cycle has enough stacked, skipping stacking";
                    "stacked" => next_cycle_stx,
                    "min" => min_stx,
                );
                continue;
            }
            let Some(account) = stacker_accounts.iter().find_map(|(sk, addr)| {
                let account = get_account(&http_origin, &addr);
                if account.locked == 0 {
                    Some((sk, addr, account))
                } else {
                    None
                }
            }) else {
                continue;
            };

            let pox_addr_tuple = clarity::vm::tests::execute(&format!(
                "{{ hashbytes: 0x{}, version: 0x{:02x} }}",
                tests::to_addr(&account.0).bytes.to_hex(),
                AddressHashMode::SerializeP2PKH as u8,
            ));

            let stacking_tx = tests::make_contract_call(
                &account.0,
                account.2.nonce,
                1000,
                &StacksAddress::burn_address(false),
                "pox-4",
                "stack-stx",
                &[
                    clarity::vm::Value::UInt(min_stx.into()),
                    pox_addr_tuple,
                    clarity::vm::Value::UInt(pox_info.current_burnchain_block_height.into()),
                    clarity::vm::Value::UInt(1),
                ],
            );
            let txid = submit_tx(&http_origin, &stacking_tx);
            info!("Submitted stacking transaction: {txid}");
            thread::sleep(Duration::from_secs(10));
        })
        .unwrap();

    run_until_burnchain_height(
        &mut btc_regtest_controller,
        &blocks_processed,
        epoch_3.start_height - 1,
        &naka_conf,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, Epoch2x miner should stop");

    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(&mut btc_regtest_controller, 60, || {
        let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
        Ok(vrf_count >= 1)
    })
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(&mut btc_regtest_controller, 60, || {
        let commits_count = commits_submitted.load(Ordering::SeqCst);
        Ok(commits_count >= 1)
    })
    .unwrap();

    info!("Bootstrapped to Epoch-3.0 boundary, mining nakamoto blocks");

    let burnchain = naka_conf.get_burnchain();
    let sortdb = burnchain.open_sortition_db(true).unwrap();

    // Mine nakamoto tenures
    for _i in 0..30 {
        if let Err(e) = next_block_and_mine_commit(
            &mut btc_regtest_controller,
            30,
            &coord_channel,
            &commits_submitted,
        ) {
            warn!(
                "Error while minting a bitcoin block and waiting for stacks-node activity: {e:?}"
            );
        }

        let tip_sn = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn()).unwrap();
        assert!(
            tip_sn.sortition,
            "The new chain tip must have had a sortition"
        );
    }

    coord_channel
        .lock()
        .expect("Mutex poisoned")
        .stop_chains_coordinator();
    run_loop_stopper.store(false, Ordering::SeqCst);

    run_loop_thread.join().unwrap();
}
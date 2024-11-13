use borsh::BorshDeserialize;
use itertools::Itertools;
use near_async::test_loop::data::{TestLoopData, TestLoopDataHandle};
use near_async::time::Duration;
use near_chain::ChainStoreAccess;
use near_chain_configs::test_genesis::TestGenesisBuilder;
use near_client::Client;
use near_o11y::testonly::init_test_logger;
use near_primitives::epoch_manager::EpochConfigStore;
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::{account_id_to_shard_uid, ShardLayout};
use near_primitives::state_record::StateRecord;
use near_primitives::types::{AccountId, BlockHeightDelta};
use near_primitives::version::{ProtocolFeature, PROTOCOL_VERSION};
use near_store::adapter::StoreAdapter;
use near_store::db::refcount::decode_value_with_rc;
use near_store::{DBCol, ShardUId};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::test_loop::builder::TestLoopBuilder;
use crate::test_loop::env::TestLoopEnv;
use crate::test_loop::utils::ONE_NEAR;
use near_client::client_actor::ClientActorInner;

fn print_and_assert_shard_accounts(client: &Client) {
    let tip = client.chain.head().unwrap();
    let epoch_id = tip.epoch_id;
    let epoch_config = client.epoch_manager.get_epoch_config(&epoch_id).unwrap();
    for shard_uid in epoch_config.shard_layout.shard_uids() {
        let chunk_extra = client.chain.get_chunk_extra(&tip.prev_block_hash, &shard_uid).unwrap();
        let trie = client
            .runtime_adapter
            .get_trie_for_shard(
                shard_uid.shard_id(),
                &tip.prev_block_hash,
                *chunk_extra.state_root(),
                false,
            )
            .unwrap();
        let mut shard_accounts = vec![];
        for item in trie.lock_for_iter().iter().unwrap() {
            let (key, value) = item.unwrap();
            let state_record = StateRecord::from_raw_key_value(key, value);
            if let Some(StateRecord::Account { account_id, .. }) = state_record {
                shard_accounts.push(account_id.to_string());
            }
        }
        println!("accounts for shard {}: {:?}", shard_uid, shard_accounts);
        assert!(!shard_accounts.is_empty());
    }
}

/// Asserts that all parent shard State is accessible via parent and children shards.
fn check_state_shard_uid_mapping_after_resharding(client: &Client, parent_shard_uid: ShardUId) {
    let tip = client.chain.head().unwrap();
    let epoch_id = tip.epoch_id;
    let epoch_config = client.epoch_manager.get_epoch_config(&epoch_id).unwrap();
    let children_shard_uids =
        epoch_config.shard_layout.get_children_shards_uids(parent_shard_uid.shard_id()).unwrap();
    assert_eq!(children_shard_uids.len(), 2);

    let store = client.chain.chain_store.store().trie_store();
    for kv in store.store().iter_raw_bytes(DBCol::State) {
        let (key, value) = kv.unwrap();
        let shard_uid = ShardUId::try_from_slice(&key[0..8]).unwrap();
        // Just after resharding, no State data must be keyed using children ShardUIds.
        assert!(!children_shard_uids.contains(&shard_uid));
        if shard_uid != parent_shard_uid {
            continue;
        }
        let node_hash = CryptoHash::try_from_slice(&key[8..]).unwrap();
        let (value, _) = decode_value_with_rc(&value);
        let parent_value = store.get(parent_shard_uid, &node_hash);
        // Parent shard data must still be accessible using parent ShardUId.
        assert_eq!(&parent_value.unwrap()[..], value.unwrap());
        // All parent shard data is available via both children shards.
        for child_shard_uid in &children_shard_uids {
            let child_value = store.get(*child_shard_uid, &node_hash);
            assert_eq!(&child_value.unwrap()[..], value.unwrap());
        }
    }
}

#[derive(Default)]
struct TestReshardingParameters {
    chunk_ranges_to_drop: HashMap<ShardUId, std::ops::Range<i64>>,
    accounts: Vec<AccountId>,
    clients: Vec<AccountId>,
    block_and_chunk_producers: Vec<AccountId>,
    initial_balance: u128,
    epoch_length: BlockHeightDelta,
    /// Custom behavior executed at every iteration of test loop.
    loop_action: Option<Box<dyn Fn(&mut TestLoopData, TestLoopDataHandle<ClientActorInner>)>>,
}

impl TestReshardingParameters {
    fn new() -> Self {
        Self::with_clients(3)
    }

    fn with_clients(num_clients: u64) -> Self {
        let num_accounts = 8;
        let initial_balance = 1_000_000 * ONE_NEAR;
        let epoch_length = 6;

        // #12195 prevents number of BPs bigger than `epoch_length`.
        assert!(num_clients > 0 && num_clients <= epoch_length);

        let accounts = (0..num_accounts)
            .map(|i| format!("account{}", i).parse().unwrap())
            .collect::<Vec<AccountId>>();

        // This piece of code creates `num_clients` from `accounts`. First client is at index 0 and
        // other clients are spaced in the accounts' space as evenly as possible.
        let clients_per_account = num_clients as f64 / accounts.len() as f64;
        let mut client_parts = 1.0 - clients_per_account;
        let clients: Vec<_> = accounts
            .iter()
            .filter(|_| {
                client_parts += clients_per_account;
                if client_parts >= 1.0 {
                    client_parts -= 1.0;
                    true
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        let block_and_chunk_producers = clients.clone();

        Self {
            accounts,
            clients,
            block_and_chunk_producers,
            initial_balance,
            epoch_length,
            ..Default::default()
        }
    }

    fn chunk_ranges_to_drop(
        mut self,
        chunk_ranges_to_drop: HashMap<ShardUId, std::ops::Range<i64>>,
    ) -> Self {
        self.chunk_ranges_to_drop = chunk_ranges_to_drop;
        self
    }

    #[allow(unused)]
    fn clients(mut self, clients: Vec<AccountId>) -> Self {
        self.clients = clients;
        self
    }

    #[allow(unused)]
    fn block_and_chunk_producers(mut self, block_and_chunk_producers: Vec<AccountId>) -> Self {
        self.block_and_chunk_producers = block_and_chunk_producers;
        self
    }

    #[allow(unused)]
    fn loop_action(
        mut self,
        loop_action: Option<Box<dyn Fn(&mut TestLoopData, TestLoopDataHandle<ClientActorInner>)>>,
    ) -> Self {
        self.loop_action = loop_action;
        self
    }
}

// Returns a callable function that, when invoked inside a test loop iteration, can force the creation of a chain fork.
#[cfg(feature = "test_features")]
fn fork_before_resharding_block(
    double_signing: bool,
) -> Box<dyn Fn(&mut TestLoopData, TestLoopDataHandle<ClientActorInner>)> {
    use near_client::client_actor::AdvProduceBlockHeightSelection;

    let done = std::cell::Cell::new(false);
    Box::new(
        move |test_loop_data: &mut TestLoopData,
              client_handle: TestLoopDataHandle<ClientActorInner>| {
            // It must happen only for the first resharding block encountered.
            if done.get() {
                return;
            }

            let client_actor = &mut test_loop_data.get_mut(&client_handle);
            let tip = client_actor.client.chain.head().unwrap();

            // We want to understand if the most recent block is a resharding block.
            // To do this check if the latest block is an epoch start and compare the two epochs' shard layouts.
            let epoch_manager = client_actor.client.epoch_manager.clone();
            let shard_layout = epoch_manager.get_shard_layout(&tip.epoch_id).unwrap();
            let next_epoch_id =
                epoch_manager.get_next_epoch_id_from_prev_block(&tip.prev_block_hash).unwrap();
            let next_shard_layout = epoch_manager.get_shard_layout(&next_epoch_id).unwrap();
            let next_block_has_new_shard_layout =
                epoch_manager.is_next_block_epoch_start(&tip.last_block_hash).unwrap()
                    && shard_layout != next_shard_layout;

            // If there's a new shard layout force a chain fork.
            if next_block_has_new_shard_layout {
                println!("creating chain fork at height {}", tip.height);
                let height_selection = if double_signing {
                    // In the double signing scenario we want a new block on top of prev block, with consecutive height.
                    AdvProduceBlockHeightSelection::NextHeightOnSelectedBlock {
                        base_block_height: tip.height - 1,
                    }
                } else {
                    // To avoid double signing skip already produced height.
                    AdvProduceBlockHeightSelection::SelectedHeightOnSelectedBlock {
                        produced_block_height: tip.height + 1,
                        base_block_height: tip.height - 1,
                    }
                };
                client_actor.adv_produce_blocks_on(3, true, height_selection);
                done.set(true);
            }
        },
    )
}

/// Base setup to check sanity of Resharding V3.
/// TODO(#11881): add the following scenarios:
/// - Nodes must not track all shards. State sync must succeed.
/// - Set up chunk validator-only nodes. State witness must pass validation.
/// - Consistent tx load. All txs must succeed.
/// - Delayed receipts, congestion control computation.
/// - Cross-shard receipts of all kinds, crossing resharding boundary.
/// - Shard layout v2 -> v2 transition.
/// - Shard layout can be taken from mainnet.
fn test_resharding_v3_base(params: TestReshardingParameters) {
    if !ProtocolFeature::SimpleNightshadeV4.enabled(PROTOCOL_VERSION) {
        return;
    }

    init_test_logger();
    let builder = TestLoopBuilder::new();

    // Prepare shard split configuration.
    let base_epoch_config_store = EpochConfigStore::for_chain_id("mainnet", None).unwrap();
    let base_protocol_version = ProtocolFeature::SimpleNightshadeV4.protocol_version() - 1;
    let mut base_epoch_config =
        base_epoch_config_store.get_config(base_protocol_version).as_ref().clone();
    base_epoch_config.validator_selection_config.shuffle_shard_assignment_for_chunk_producers =
        false;
    if !params.chunk_ranges_to_drop.is_empty() {
        base_epoch_config.block_producer_kickout_threshold = 0;
        base_epoch_config.chunk_producer_kickout_threshold = 0;
        base_epoch_config.chunk_validator_only_kickout_threshold = 0;
    }
    #[allow(deprecated)]
    let base_shard_layout = ShardLayout::v1(vec!["account3".parse().unwrap()], None, 3);
    base_epoch_config.shard_layout = base_shard_layout.clone();
    let new_boundary_account = "account6".parse().unwrap();
    let mut epoch_config = base_epoch_config.clone();
    let parent_shard_uid = account_id_to_shard_uid(&new_boundary_account, &base_shard_layout);

    epoch_config.shard_layout =
        ShardLayout::derive_shard_layout(&base_shard_layout, new_boundary_account);
    tracing::info!(target: "test", ?base_shard_layout, new_shard_layout=?epoch_config.shard_layout, "shard layout");

    let expected_num_shards = epoch_config.shard_layout.shard_ids().count();
    let epoch_config_store = EpochConfigStore::test(BTreeMap::from_iter(vec![
        (base_protocol_version, Arc::new(base_epoch_config)),
        (base_protocol_version + 1, Arc::new(epoch_config)),
    ]));

    let mut genesis_builder = TestGenesisBuilder::new();
    genesis_builder
        .genesis_time_from_clock(&builder.clock())
        .shard_layout(base_shard_layout)
        .protocol_version(base_protocol_version)
        .epoch_length(params.epoch_length)
        .validators_desired_roles(
            &params
                .block_and_chunk_producers
                .iter()
                .map(|account_id| account_id.as_str())
                .collect_vec(),
            &[],
        );
    for account in &params.accounts {
        genesis_builder.add_user_account_simple(account.clone(), params.initial_balance);
    }
    let (genesis, _) = genesis_builder.build();

    let TestLoopEnv { mut test_loop, datas: node_datas, tempdir } = builder
        .genesis(genesis)
        .epoch_config_store(epoch_config_store)
        .clients(params.clients)
        .drop_protocol_upgrade_chunks(
            base_protocol_version + 1,
            params.chunk_ranges_to_drop.clone(),
        )
        .track_all_shards()
        .build();

    let client_handle = node_datas[0].client_sender.actor_handle();
    let latest_block_height = std::cell::Cell::new(0u64);
    let success_condition = |test_loop_data: &mut TestLoopData| -> bool {
        params.loop_action.as_ref().map(|action| action(test_loop_data, client_handle.clone()));

        let client = &test_loop_data.get(&client_handle).client;
        let tip = client.chain.head().unwrap();

        // Check that all chunks are included.
        let block_header = client.chain.get_block_header(&tip.last_block_hash).unwrap();
        if latest_block_height.get() < tip.height {
            if latest_block_height.get() == 0 {
                println!("State before resharding:");
                print_and_assert_shard_accounts(client);
            }
            latest_block_height.set(tip.height);
            println!("block: {} chunks: {:?}", tip.height, block_header.chunk_mask());
            if params.chunk_ranges_to_drop.is_empty() {
                assert!(block_header.chunk_mask().iter().all(|chunk_bit| *chunk_bit));
            }
        }

        // Return true if we passed an epoch with increased number of shards.
        let epoch_height =
            client.epoch_manager.get_epoch_height_from_prev_block(&tip.prev_block_hash).unwrap();
        assert!(epoch_height < 6);
        let prev_epoch_id =
            client.epoch_manager.get_prev_epoch_id_from_prev_block(&tip.prev_block_hash).unwrap();
        let epoch_config = client.epoch_manager.get_epoch_config(&prev_epoch_id).unwrap();
        if epoch_config.shard_layout.shard_ids().count() != expected_num_shards {
            return false;
        }

        println!("State after resharding:");
        print_and_assert_shard_accounts(client);
        check_state_shard_uid_mapping_after_resharding(&client, parent_shard_uid);
        return true;
    };

    test_loop.run_until(
        success_condition,
        // Give enough time to produce ~7 epochs.
        Duration::seconds((7 * params.epoch_length) as i64),
    );

    TestLoopEnv { test_loop, datas: node_datas, tempdir }
        .shutdown_and_drain_remaining_events(Duration::seconds(20));
}

#[test]
fn test_resharding_v3() {
    test_resharding_v3_base(TestReshardingParameters::new());
}

#[test]
fn test_resharding_v3_drop_chunks_before() {
    let chunk_ranges_to_drop = HashMap::from([(ShardUId { shard_id: 1, version: 3 }, -2..0)]);
    test_resharding_v3_base(
        TestReshardingParameters::new().chunk_ranges_to_drop(chunk_ranges_to_drop),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_after() {
    let chunk_ranges_to_drop = HashMap::from([(ShardUId { shard_id: 2, version: 3 }, 0..2)]);
    test_resharding_v3_base(
        TestReshardingParameters::new().chunk_ranges_to_drop(chunk_ranges_to_drop),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_before_and_after() {
    let chunk_ranges_to_drop = HashMap::from([(ShardUId { shard_id: 0, version: 3 }, -2..2)]);
    test_resharding_v3_base(
        TestReshardingParameters::new().chunk_ranges_to_drop(chunk_ranges_to_drop),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_all() {
    let chunk_ranges_to_drop = HashMap::from([
        (ShardUId { shard_id: 0, version: 3 }, -1..2),
        (ShardUId { shard_id: 1, version: 3 }, -3..0),
        (ShardUId { shard_id: 2, version: 3 }, 0..3),
        (ShardUId { shard_id: 3, version: 3 }, 0..1),
    ]);
    test_resharding_v3_base(
        TestReshardingParameters::new().chunk_ranges_to_drop(chunk_ranges_to_drop),
    );
}

#[test]
// TODO(resharding): fix nearcore and un-ignore this test
#[ignore]
#[cfg(feature = "test_features")]
fn test_resharding_v3_resharding_block_in_fork() {
    test_resharding_v3_base(
        TestReshardingParameters::with_clients(1)
            .loop_action(Some(fork_before_resharding_block(false))),
    );
}

#[test]
// TODO(resharding): fix nearcore and un-ignore this test
// TODO(resharding): duplicate this test so that in one case resharding is performed on block
//                   B(height=13) and in another case resharding is performed on block B'(height=13)
#[ignore]
#[cfg(feature = "test_features")]
fn test_resharding_v3_double_sign_resharding_block() {
    test_resharding_v3_base(
        TestReshardingParameters::with_clients(1)
            .loop_action(Some(fork_before_resharding_block(true))),
    );
}

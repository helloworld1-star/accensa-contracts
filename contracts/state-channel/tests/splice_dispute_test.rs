extern crate std;

use accensa_common::Error;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, BytesN, Env,
};
use state_channel::{ChannelPhase, StateChannel, StateChannelClient, StateUpdate};

#[test]
fn test_splice_blocked_during_dispute() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sender = Address::generate(&env);
    let receiver = Address::generate(&env);

    let token = env.register_stellar_asset_contract_v2(admin).address();
    let contract = env.register(StateChannel, ());
    let client = StateChannelClient::new(&env, &contract);
    client.initialize(&token);

    let deposit: i128 = 1_000;
    let challenge_period: u32 = 100;
    StellarAssetClient::new(&env, &token).mint(&sender, &deposit);

    let pk = BytesN::from_array(&env, &[9u8; 32]);
    let channel_id = client.open_channel(&sender, &receiver, &pk, &deposit, &challenge_period);

    let state = StateUpdate { nonce: 1, balance: 100 };
    let sig = BytesN::from_array(&env, &[0u8; 64]);

    // Close channel first so it can be disputed
    client.close_channel(&channel_id, &state, &sig);

    // Initiate a dispute on the closed channel
    client.dispute(&channel_id, &state, &sig);
    assert_eq!(
        client.get_channel(&channel_id).phase,
        ChannelPhase::Disputed
    );

    // Verify splice_out during active dispute fails with Error::ChannelNotOpen (or relevant Error)
    let res_out = client.try_splice_out(&channel_id, &100);
    assert!(res_out.is_err());

    // Verify splice_in during active dispute fails
    let res_in = client.try_splice_in(&channel_id, &100);
    assert!(res_in.is_err());

    // Advance ledger past challenge window and finalize dispute
    let current_ledger = env.ledger().sequence();
    env.ledger()
        .set_sequence(current_ledger + challenge_period + 1);

    client.finalize_dispute(&channel_id);
    assert_eq!(
        client.get_channel(&channel_id).phase,
        ChannelPhase::Finalized
    );
}

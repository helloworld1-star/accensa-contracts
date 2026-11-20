extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Events as _},
    token::StellarAssetClient,
    Address, BytesN, Env,
};
use state_channel::{StateChannel, StateChannelClient, ChannelOpened};

const DEPOSIT: i128 = 1_000;

fn setup() -> (Env, StateChannelClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let receiver = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    StellarAssetClient::new(&env, &token).mint(&sender, &DEPOSIT);

    let contract = env.register(StateChannel, ());
    let client = StateChannelClient::new(&env, &contract);
    client.initialize(&token);

    (env, client, sender, receiver)
}

fn pk(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[7u8; 32])
}

#[test]
fn test_challenge_period_clamping() {
    let (env, client, sender, receiver) = setup();

    let default_period = client.get_default_challenge_period();
    let max_period = client.get_max_challenge_period();

    // 1. challenge_period = 0 -> clamped to default
    let channel_id_zero = client.open_channel(&sender, &receiver, &pk(&env), &DEPOSIT, &0);
    let channel_zero = client.get_channel(&channel_id_zero);
    assert_eq!(channel_zero.challenge_period, default_period);

    let events = env.events().all();
    let last_event = events.last().unwrap();
    let decoded_event: ChannelOpened = ChannelOpened::from_val(&env, &last_event);
    assert_eq!(decoded_event.challenge_period, default_period);

    // 2. challenge_period = u32::MAX -> clamped to max
    let channel_id_max = client.open_channel(&sender, &receiver, &pk(&env), &DEPOSIT, &u32::MAX);
    let channel_max = client.get_channel(&channel_id_max);
    assert_eq!(channel_max.challenge_period, max_period);

    let events = env.events().all();
    let last_event = events.last().unwrap();
    let decoded_event_max: ChannelOpened = ChannelOpened::from_val(&env, &last_event);
    assert_eq!(decoded_event_max.challenge_period, max_period);
}

//! Pins the challenge-period clamping performed by `open_channel`.
//!
//! Events are captured immediately after opening each channel because the
//! Soroban event test utilities expose events from the latest invocation.

use soroban_sdk::{
    testutils::{Address as _, Events as _},
    token::StellarAssetClient,
    Address, BytesN, Env, Event,
};
use state_channel::{StateChannel, StateChannelClient};

const DEPOSIT: i128 = 1_000;

// The contract keeps its event type private. Mirroring its event definition
// lets the test use the SDK's event encoder to compare the complete raw event.
#[soroban_sdk::contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
struct ChannelOpenedEvent {
    #[topic]
    channel_id: u64,
    sender: Address,
    receiver: Address,
    amount: i128,
    challenge_period: u32,
}

struct Harness {
    env: Env,
    client: StateChannelClient<'static>,
    contract: Address,
    sender: Address,
    receiver: Address,
}

fn setup() -> Harness {
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

    Harness {
        env,
        client,
        contract,
        sender,
        receiver,
    }
}

fn assert_open_channel_reports_period(h: &Harness, requested: u32, expected: u32) {
    let sender_pubkey = BytesN::from_array(&h.env, &[7u8; 32]);
    let channel_id =
        h.client
            .open_channel(&h.sender, &h.receiver, &sender_pubkey, &DEPOSIT, &requested);
    assert_eq!(channel_id, 1);

    // Capture the open_channel events before get_channel becomes the latest
    // contract invocation.
    let events = h.env.events().all().filter_by_contract(&h.contract);

    let stored = h.client.get_channel(&channel_id);
    assert_eq!(stored.challenge_period, expected);

    let expected_event = ChannelOpenedEvent {
        channel_id,
        sender: h.sender.clone(),
        receiver: h.receiver.clone(),
        amount: DEPOSIT,
        challenge_period: expected,
    };
    assert_eq!(events.events().len(), 1);
    assert_eq!(
        &events.events()[0],
        &expected_event.to_xdr(&h.env, &h.contract)
    );
}

#[test]
fn zero_challenge_period_uses_default_in_storage_and_event() {
    let h = setup();
    let default = h.client.get_default_challenge_period();

    assert_open_channel_reports_period(&h, 0, default);
}

#[test]
fn default_challenge_period_is_unchanged_in_storage_and_event() {
    let h = setup();
    let default = h.client.get_default_challenge_period();

    assert_open_channel_reports_period(&h, default, default);
}

#[test]
fn maximum_challenge_period_is_unchanged_in_storage_and_event() {
    let h = setup();
    let maximum = h.client.get_max_challenge_period();

    assert_open_channel_reports_period(&h, maximum, maximum);
}

#[test]
fn excessive_challenge_period_uses_maximum_in_storage_and_event() {
    let h = setup();
    let maximum = h.client.get_max_challenge_period();

    assert_open_channel_reports_period(&h, u32::MAX, maximum);
}

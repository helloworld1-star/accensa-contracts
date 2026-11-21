use accensa_common::Error;
use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env,
};
use state_channel::{ChannelPhase, StateChannel, StateChannelClient, StateUpdate};

const DEPOSIT: i128 = 1_000;
const CHALLENGE_PERIOD: u32 = 100;
const CLOSE_LEDGER: u32 = 10;
const DISPUTE_LEDGER: u32 = 20;
const EVIDENCE_LEDGER: u32 = 30;
const BOUNTY_BPS: u32 = 1_000;

struct Setup {
    env: Env,
    client: StateChannelClient<'static>,
    token: Address,
    sender: Address,
    receiver: Address,
    sender_sk: SigningKey,
    sender_pk: BytesN<32>,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sender = Address::generate(&env);
    let receiver = Address::generate(&env);
    let sender_sk = SigningKey::from_bytes(&[1u8; 32]);
    let sender_pk = BytesN::from_array(&env, &sender_sk.verifying_key().to_bytes());

    let token = env.register_stellar_asset_contract_v2(admin).address();
    let contract = env.register(StateChannel, ());
    let client = StateChannelClient::new(&env, &contract);
    client.initialize(&token);
    StellarAssetClient::new(&env, &token).mint(&sender, &1_000_000);

    Setup {
        env,
        client,
        token,
        sender,
        receiver,
        sender_sk,
        sender_pk,
    }
}

impl Setup {
    fn state(&self, nonce: u64, balance: i128) -> StateUpdate {
        StateUpdate { nonce, balance }
    }

    fn sign_with(&self, key: &SigningKey, state: &StateUpdate) -> BytesN<64> {
        let mut message = [0u8; 56];
        message[..32].copy_from_slice(&self.sender_pk.to_array());
        message[32..40].copy_from_slice(&state.nonce.to_be_bytes());
        message[40..56].copy_from_slice(&state.balance.to_be_bytes());
        BytesN::from_array(&self.env, &key.sign(&message).to_bytes())
    }

    fn sign(&self, state: &StateUpdate) -> BytesN<64> {
        self.sign_with(&self.sender_sk, state)
    }

    fn open_channel(&self) -> u64 {
        self.client.open_channel(
            &self.sender,
            &self.receiver,
            &self.sender_pk,
            &DEPOSIT,
            &CHALLENGE_PERIOD,
        )
    }

    /// Close with a stale state, then dispute it with a newer receiver balance.
    fn begin_dispute(&self, channel_id: u64) {
        self.env.ledger().set_sequence_number(CLOSE_LEDGER);
        let stale = self.state(1, 100);
        self.client
            .close_channel(&channel_id, &stale, &self.sign(&stale));

        self.env.ledger().set_sequence_number(DISPUTE_LEDGER);
        let disputed = self.state(2, 300);
        self.client
            .dispute(&channel_id, &disputed, &self.sign(&disputed));
    }

    fn balance(&self, address: &Address) -> i128 {
        TokenClient::new(&self.env, &self.token).balance(address)
    }
}

#[test]
fn watchtower_counter_evidence_rearms_window_and_records_bounty_defender() {
    let s = setup();
    let channel_id = s.open_channel();
    s.client.set_watchtower_bounty(&channel_id, &BOUNTY_BPS);
    s.begin_dispute(channel_id);

    assert_eq!(
        s.client.get_channel(&channel_id).disputed_at,
        DISPUTE_LEDGER
    );

    let watchtower = Address::generate(&s.env);
    // Late counter-evidence exercises the dispute-window extension. An early
    // counter-proof updates the state without postponing the original deadline.
    let late_evidence_ledger = DISPUTE_LEDGER + CHALLENGE_PERIOD - 1;
    s.env.ledger().set_sequence_number(late_evidence_ledger);
    let freshest = s.state(3, 500);
    s.client
        .watchtower_counter_evidence(&channel_id, &freshest, &s.sign(&freshest), &watchtower);

    let defended = s.client.get_channel(&channel_id);
    assert_eq!(defended.phase, ChannelPhase::Disputed);
    assert_eq!(defended.nonce, 3);
    assert_eq!(defended.balance, 500);

    // The deadline created by the original dispute was ledger 120. The late
    // watchtower proof re-arms the window, so that deadline no longer permits
    // settlement.
    s.env
        .ledger()
        .set_sequence_number(DISPUTE_LEDGER + CHALLENGE_PERIOD + 1);
    assert_eq!(
        s.client.try_finalize_dispute(&channel_id),
        Err(Ok(Error::ChallengeActive))
    );

    s.env
        .ledger()
        .set_sequence_number(late_evidence_ledger + CHALLENGE_PERIOD + 1);
    s.client.finalize_dispute(&channel_id);

    // Only the defender recorded by watchtower_counter_evidence can receive
    // this split: 10% of 500 goes to the watchtower and 450 to the receiver.
    assert_eq!(s.balance(&watchtower), 50);
    assert_eq!(s.balance(&s.receiver), 450);
    assert_eq!(s.balance(&s.sender), 999_500);
    assert_eq!(
        s.client.get_channel(&channel_id).phase,
        ChannelPhase::Finalized
    );
}

#[test]
fn watchtower_counter_evidence_rejects_invalid_signature_without_recording_defender() {
    let s = setup();
    let channel_id = s.open_channel();
    s.client.set_watchtower_bounty(&channel_id, &BOUNTY_BPS);
    s.begin_dispute(channel_id);

    let watchtower = Address::generate(&s.env);
    let freshest = s.state(3, 500);
    let wrong_key = SigningKey::from_bytes(&[2u8; 32]);
    let forged_signature = s.sign_with(&wrong_key, &freshest);

    s.env.ledger().set_sequence_number(EVIDENCE_LEDGER);
    let result = s.client.try_watchtower_counter_evidence(
        &channel_id,
        &freshest,
        &forged_signature,
        &watchtower,
    );

    match result {
        Err(Ok(error)) => assert_eq!(error, Error::InvalidSignature),
        // An individually forged Ed25519 signature traps in host crypto; the
        // generated try_ client exposes that rejection as an invocation error.
        Err(Err(_)) => {}
        Ok(_) => panic!("forged watchtower evidence was accepted"),
    }

    let unchanged = s.client.get_channel(&channel_id);
    assert_eq!(unchanged.phase, ChannelPhase::Disputed);
    assert_eq!(unchanged.nonce, 2);
    assert_eq!(unchanged.balance, 300);
    assert_eq!(unchanged.disputed_at, DISPUTE_LEDGER);

    // Settlement after the original window confirms that the rejected call did
    // not leave a defender eligible for the configured bounty.
    s.env
        .ledger()
        .set_sequence_number(DISPUTE_LEDGER + CHALLENGE_PERIOD + 1);
    s.client.finalize_dispute(&channel_id);

    assert_eq!(s.balance(&watchtower), 0);
    assert_eq!(s.balance(&s.receiver), 300);
    assert_eq!(s.balance(&s.sender), 999_700);
}

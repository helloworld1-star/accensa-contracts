//! A dispute freezes channel capacity until the disputed state is settled.

use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env,
};
use state_channel::{ChannelPhase, Error, StateChannel, StateChannelClient, StateUpdate};

const DEPOSIT: i128 = 1_000;
const CHALLENGE: u32 = 100;
const DISPUTED_BALANCE: i128 = 600;

struct Setup {
    env: Env,
    client: StateChannelClient<'static>,
    contract: Address,
    token: Address,
    sender: Address,
    receiver: Address,
    sender_sk: SigningKey,
}

fn setup() -> Setup {
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

    Setup {
        env,
        client,
        contract,
        token,
        sender,
        receiver,
        sender_sk: SigningKey::from_bytes(&[5u8; 32]),
    }
}

impl Setup {
    fn open(&self) -> u64 {
        let sender_pubkey =
            BytesN::from_array(&self.env, &self.sender_sk.verifying_key().to_bytes());
        self.client.open_channel(
            &self.sender,
            &self.receiver,
            &sender_pubkey,
            &DEPOSIT,
            &CHALLENGE,
        )
    }

    fn state(&self, nonce: u64, balance: i128) -> StateUpdate {
        StateUpdate { nonce, balance }
    }

    fn sign(&self, state: &StateUpdate) -> BytesN<64> {
        let sender_pubkey =
            BytesN::from_array(&self.env, &self.sender_sk.verifying_key().to_bytes());
        let mut message = [0u8; 56];
        message[..32].copy_from_slice(&sender_pubkey.to_array());
        message[32..40].copy_from_slice(&state.nonce.to_be_bytes());
        message[40..56].copy_from_slice(&state.balance.to_be_bytes());
        BytesN::from_array(&self.env, &self.sender_sk.sign(&message).to_bytes())
    }

    fn balance(&self, address: &Address) -> i128 {
        TokenClient::new(&self.env, &self.token).balance(address)
    }
}

#[test]
fn dispute_blocks_splices_and_finalize_settles_the_disputed_state() {
    let s = setup();
    let channel_id = s.open();

    assert_eq!(s.client.get_channel(&channel_id).phase, ChannelPhase::Open);

    // A unilateral close starts the challenge window in which a signed state
    // can be disputed; disputing a channel that is still open is rejected.
    s.client.close_channel(&channel_id);
    assert_eq!(
        s.client.get_channel(&channel_id).phase,
        ChannelPhase::Closed
    );

    let disputed_state = s.state(1, DISPUTED_BALANCE);
    s.client
        .dispute(&channel_id, &disputed_state, &s.sign(&disputed_state));

    let disputed_channel = s.client.get_channel(&channel_id);
    assert_eq!(disputed_channel.phase, ChannelPhase::Disputed);
    assert_eq!(disputed_channel.nonce, 1);
    assert_eq!(disputed_channel.balance, DISPUTED_BALANCE);

    // Neither splice direction may resize capacity or move contested escrow.
    assert_eq!(
        s.client.try_splice_out(&channel_id, &400),
        Err(Ok(Error::InvalidPhase))
    );
    assert_eq!(
        s.client.try_splice_in(&channel_id, &100),
        Err(Ok(Error::InvalidPhase))
    );

    let blocked_channel = s.client.get_channel(&channel_id);
    assert_eq!(blocked_channel.phase, ChannelPhase::Disputed);
    assert_eq!(blocked_channel.amount, DEPOSIT);
    assert_eq!(blocked_channel.nonce, 1);
    assert_eq!(blocked_channel.balance, DISPUTED_BALANCE);
    assert_eq!(s.balance(&s.contract), DEPOSIT);
    assert_eq!(s.balance(&s.sender), 0);
    assert_eq!(s.balance(&s.receiver), 0);

    // Once the dispute deadline is strictly in the past, finalization settles
    // exactly the disputed state and returns the uncommitted escrow.
    let finalization_ledger = blocked_channel.disputed_at + blocked_channel.challenge_period + 1;
    s.env.ledger().set_sequence_number(finalization_ledger);
    s.client.finalize_dispute(&channel_id);

    assert_eq!(
        s.client.get_channel(&channel_id).phase,
        ChannelPhase::Finalized
    );
    assert_eq!(s.balance(&s.receiver), DISPUTED_BALANCE);
    assert_eq!(s.balance(&s.sender), DEPOSIT - DISPUTED_BALANCE);
    assert_eq!(s.balance(&s.contract), 0);
}

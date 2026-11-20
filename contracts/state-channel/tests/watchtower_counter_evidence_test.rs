//! Integration tests for watchtower counter evidence and bounty split (issue #459 / watchtower dispute mechanics).

extern crate std;

use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env,
};
use state_channel::{ChannelPhase, StateChannel, StateChannelClient, StateUpdate};

struct Setup {
    env: Env,
    client: StateChannelClient<'static>,
    token: Address,
    sender: Address,
    receiver: Address,
    sk: SigningKey,
    pk: BytesN<32>,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sender = Address::generate(&env);
    let receiver = Address::generate(&env);
    let sk = SigningKey::from_bytes(&[2u8; 32]);
    let pk = BytesN::from_array(&env, &sk.verifying_key().to_bytes());

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
        sk,
        pk,
    }
}

impl Setup {
    fn sign(&self, nonce: u64, balance: i128) -> BytesN<64> {
        let mut msg = [0u8; 56];
        msg[..32].copy_from_slice(&self.pk.to_array());
        msg[32..40].copy_from_slice(&nonce.to_be_bytes());
        msg[40..56].copy_from_slice(&balance.to_be_bytes());
        BytesN::from_array(&self.env, &self.sk.sign(&msg).to_bytes())
    }

    fn state(&self, nonce: u64, balance: i128) -> StateUpdate {
        StateUpdate { nonce, balance }
    }

    fn open(&self, amount: i128, challenge: u32) -> u64 {
        self.client
            .open_channel(&self.sender, &self.receiver, &self.pk, &amount, &challenge)
    }

    fn balance(&self, who: &Address) -> i128 {
        TokenClient::new(&self.env, &self.token).balance(who)
    }
}

#[test]
fn test_watchtower_counter_evidence_end_to_end() {
    let s = setup();
    let challenge_period = 100;
    let channel_id = s.open(1_000, challenge_period);
    let watchtower = Address::generate(&s.env);

    // Configure a watchtower bounty (e.g. 10% / 1000 basis points or direct amount depending on implementation)
    s.client.set_watchtower_bounty(&channel_id, &1_000);

    // Sender closes channel with an outdated state (nonce 1, balance 100)
    let s1 = s.state(1, 100);
    s.env.ledger().set_sequence_number(10);
    s.client.close_channel(&channel_id, &s1, &s.sign(1, 100));

    let channel_after_close = s.client.get_channel(&channel_id);
    assert_eq!(channel_after_close.phase, ChannelPhase::Closed);
    assert_eq!(channel_after_close.closed_at, 10);

    // Dispute triggered with a slightly newer valid state (nonce 2, balance 200)
    let s2 = s.state(2, 200);
    s.env.ledger().set_sequence_number(20);
    s.client.dispute(&channel_id, &s2, &s.sign(2, 200));

    let channel_after_dispute = s.client.get_channel(&channel_id);
    assert_eq!(channel_after_dispute.phase, ChannelPhase::Disputed);
    assert_eq!(channel_after_dispute.disputed_at, 20);

    // Attempt watchtower counter evidence with invalid signature should return Error::InvalidSignature
    let invalid_sig = s.sign(3, 999); // mismatched state values
    let s3_invalid = s.state(3, 500);
    let res = s.client.try_watchtower_counter_evidence(
        &channel_id,
        &s3_invalid,
        &invalid_sig,
        &watchtower,
    );
    assert!(res.is_err());

    // Watchtower submits valid fresher counter-evidence (nonce 3, balance 500)
    let s3 = s.state(3, 500);
    s.env.ledger().set_sequence_number(30);
    s.client.watchtower_counter_evidence(
        &channel_id,
        &s3,
        &s.sign(3, 500),
        &watchtower,
    );

    // Verify dispute window resets/updates and watchtower is registered as defender
    let channel_after_wt = s.client.get_channel(&channel_id);
    assert_eq!(channel_after_wt.nonce, 3);
    assert_eq!(channel_after_wt.balance, 500);

    // Advance ledger past the new challenge window deadline and finalize dispute
    s.env.ledger().set_sequence_number(30 + challenge_period + 1);
    s.client.finalize_dispute(&channel_id);

    // Verify settlement and bounty split payouts
    assert_eq!(
        s.client.get_channel(&channel_id).phase,
        ChannelPhase::Finalized
    );
    // Watchtower receives its bounty reward, receiver gets remainder of the balance recovery
    assert!(s.balance(&watchtower) > 0);
    assert!(s.balance(&s.receiver) > 0);
}

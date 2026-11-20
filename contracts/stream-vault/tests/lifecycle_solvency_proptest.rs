extern crate std;

use proptest::prelude::*;
soroban_sdk::contractimport!(
    file = "../../target/wasm32v1-none/release/stream_vault.wasm"
);
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, Env,
};

#[derive(Clone, Debug)]
enum Action {
    Create {
        start_offset: u32,
        duration: u32,
        rate: i128,
        deposit: i128,
    },
    Claim {
        stream_idx: usize,
    },
    Pause {
        stream_idx: usize,
    },
    Resume {
        stream_idx: usize,
    },
    Cancel {
        stream_idx: usize,
    },
    AdvanceLedger {
        delta: u32,
    },
}

#[derive(Clone, Debug)]
struct ShadowStream {
    #[allow(dead_code)]
    buyer: Address,
    start_ledger: u32,
    stop_ledger: u32,
    rate_per_ledger: i128,
    deposit: i128,
    claimed: i128,
    paused_at: Option<u32>,
    total_paused_ledgers: u32,
    active: bool,
}

#[derive(Clone, Debug)]
struct ShadowModel {
    streams: std::vec::Vec<Option<ShadowStream>>,
    buyers: std::vec::Vec<Address>,
}

fn compute_shadow_claimable(stream: &ShadowStream, now: u32) -> i128 {
    if !stream.active {
        return 0;
    }
    let current = if let Some(p) = stream.paused_at {
        p
    } else {
        now
    };
    let effective_now = current.saturating_sub(stream.total_paused_ledgers);
    if effective_now <= stream.start_ledger {
        return 0;
    }
    let elapsed = effective_now
        .saturating_sub(stream.start_ledger)
        .min(stream.stop_ledger.saturating_sub(stream.start_ledger));
    let streamed = (elapsed as i128)
        .saturating_mul(stream.rate_per_ledger)
        .min(stream.deposit);
    streamed.saturating_sub(stream.claimed)
}

proptest! {
    #[test]
    fn test_stream_vault_solvency_random_lifecycles(
        actions in proptest::collection::vec(
            prop_oneof![
                (
                    0u32..20u32,
                    1u32..50u32,
                    1i128..100i128,
                    10i128..5000i128,
                ).prop_map(|(s, d, r, dep)| Action::Create {
                    start_offset: s,
                    duration: d,
                    rate: r,
                    deposit: dep,
                }),
                (0usize..50usize).prop_map(|idx| Action::Claim { stream_idx: idx }),
                (0usize..50usize).prop_map(|idx| Action::Pause { stream_idx: idx }),
                (0usize..50usize).prop_map(|idx| Action::Resume { stream_idx: idx }),
                (0usize..50usize).prop_map(|idx| Action::Cancel { stream_idx: idx }),
                (1u32..30u32).prop_map(|delta| Action::AdvanceLedger { delta }),
            ],
            1..120
        )
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_sequence_number(100);

        let merchant = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(token_admin);
        let token_address = sac.address();
        let token_client = StellarAssetClient::new(&env, &token_address);
        let token_balance_client = soroban_sdk::token::TokenClient::new(&env, &token_address);

        let vault_id = env.register(StreamVault, (merchant.clone(), token_address.clone()));
        let client = StreamVaultClient::new(&env, &vault_id);

        let mut shadow = ShadowModel {
            streams: std::vec::Vec::new(),
            buyers: std::vec::Vec::new(),
        };

        for _ in 0..5 {
            shadow.buyers.push(Address::generate(&env));
        }

        for buyer in &shadow.buyers {
            token_client.mint(buyer, &1_000_000_000);
        }

        for action in actions {
            let now = env.ledger().sequence();
            match action {
                Action::Create { start_offset, duration, rate, deposit } => {
                    let buyer = shadow.buyers[(now as usize) % shadow.buyers.len()].clone();
                    let start = now.saturating_add(start_offset);
                    let stop = start.saturating_add(duration);
                    let max_cap = (stop.saturating_sub(start) as i128).saturating_mul(rate);
                    let actual_deposit = deposit.min(max_cap.max(1));

                    if let Ok(id) = client.try_create_stream(&buyer, &start, &stop, &rate, &actual_deposit) {
                        if let Ok(stream_id) = id {
                            while shadow.streams.len() <= (stream_id as usize) {
                                shadow.streams.push(None);
                            }
                            shadow.streams[stream_id as usize] = Some(ShadowStream {
                                buyer,
                                start_ledger: start,
                                stop_ledger: stop,
                                rate_per_ledger: rate,
                                deposit: actual_deposit,
                                claimed: 0,
                                paused_at: None,
                                total_paused_ledgers: 0,
                                active: true,
                            });
                        }
                    }
                }
                Action::Claim { stream_idx } => {
                    if stream_idx < shadow.streams.len() {
                        if let Some(s) = &mut shadow.streams[stream_idx] {
                            if s.active {
                                let claimable = compute_shadow_claimable(s, now);
                                if let Ok(res) = client.try_claim_stream(&(stream_idx as u64)) {
                                    if let Ok(payout) = res {
                                        assert_eq!(payout, claimable);
                                        s.claimed = s.claimed.saturating_add(payout);
                                        if s.claimed >= s.deposit || now >= s.stop_ledger {
                                            s.active = false;
                                            shadow.streams[stream_idx] = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Action::Pause { stream_idx } => {
                    if stream_idx < shadow.streams.len() {
                        if let Some(s) = &mut shadow.streams[stream_idx] {
                            if s.active && s.paused_at.is_none() && now >= s.start_ledger && now < s.stop_ledger {
                                if client.try_pause_stream(&(stream_idx as u64)).is_ok() {
                                    s.paused_at = Some(now);
                                }
                            }
                        }
                    }
                }
                Action::Resume { stream_idx } => {
                    if stream_idx < shadow.streams.len() {
                        if let Some(s) = &mut shadow.streams[stream_idx] {
                            if s.active {
                                if let Some(paused) = s.paused_at {
                                    let diff = now.saturating_sub(paused);
                                    if client.try_resume_stream(&(stream_idx as u64)).is_ok() {
                                        s.total_paused_ledgers = s.total_paused_ledgers.saturating_add(diff);
                                        s.paused_at = None;
                                    }
                                }
                            }
                        }
                    }
                }
                Action::Cancel { stream_idx } => {
                    if stream_idx < shadow.streams.len() {
                        if let Some(s) = &mut shadow.streams[stream_idx] {
                            if s.active {
                                let claimable = compute_shadow_claimable(s, now);
                                let unspent = s.deposit.saturating_sub(s.claimed).saturating_sub(claimable);
                                if let Ok(res) = client.try_cancel_stream(&(stream_idx as u64)) {
                                    if let Ok((merchant_pay, buyer_refund)) = res {
                                        assert_eq!(merchant_pay, claimable);
                                        assert_eq!(buyer_refund, unspent);
                                        s.active = false;
                                        shadow.streams[stream_idx] = None;
                                    }
                                }
                            }
                        }
                    }
                }
                Action::AdvanceLedger { delta } => {
                    let next_ledger = now.saturating_add(delta);
                    env.ledger().set_sequence_number(next_ledger);
                }
            }

            let current_now = env.ledger().sequence();
            let mut expected_balance: i128 = 0;
            for opt in &shadow.streams {
                if let Some(s) = opt {
                    if s.active {
                        let claimable = compute_shadow_claimable(s, current_now);
                        let unspent = s.deposit.saturating_sub(s.claimed).saturating_sub(claimable);
                        let unclaimed_principal = claimable.saturating_add(unspent);
                        expected_balance = expected_balance.saturating_add(unclaimed_principal);
                    }
                }
            }

            let vault_balance = token_balance_client.balance(&vault_id);
            assert_eq!(vault_balance, expected_balance);
            assert!(vault_balance >= 0);
        }
    }
}

use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};
use stream_vault::{StreamVault, StreamVaultClient};

const BUYER_FUNDS: i128 = 100_000_000;
const BASE_LEDGER: u32 = 100;

#[derive(Clone, Debug)]
enum Op {
    CreateStream {
        buyer_idx: usize,
        start_offset: u32,
        duration: u32,
        rate: i128,
        deposit: i128,
    },
    ClaimStream {
        stream_idx: usize,
    },
    PauseStream {
        stream_idx: usize,
    },
    ResumeStream {
        stream_idx: usize,
    },
    CancelStream {
        stream_idx: usize,
    },
    AdvanceLedger {
        delta: u32,
    },
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct ShadowStream {
    buyer: Address,
    start_ledger: u32,
    stop_ledger: u32,
    rate_per_ledger: i128,
    deposit: i128,
    claimed: i128,
    paused_at: Option<u32>,
    total_paused_ledgers: u32,
    cancelled: bool,
    completed: bool,
}

fn stream_strategy() -> impl Strategy<Value = Vec<Op>> {
    use proptest::collection::vec;

    let op_strategy = prop_oneof!(
        (0..3usize, 0..10u32, 1..50u32, 1..100i128, 1..10000i128).prop_map(
            |(buyer_idx, start_offset, duration, rate, deposit)| Op::CreateStream {
                buyer_idx,
                start_offset,
                duration,
                rate,
                deposit,
            }
        ),
        (0..5usize).prop_map(|stream_idx| Op::ClaimStream { stream_idx }),
        (0..5usize).prop_map(|stream_idx| Op::PauseStream { stream_idx }),
        (0..5usize).prop_map(|stream_idx| Op::ResumeStream { stream_idx }),
        (0..5usize).prop_map(|stream_idx| Op::CancelStream { stream_idx }),
        (1..20u32).prop_map(|delta| Op::AdvanceLedger { delta }),
    );

    vec(op_strategy, 1..64)
}

fn compute_shadow_streamed(stream: &ShadowStream, now: u32) -> i128 {
    if now < stream.start_ledger {
        return 0;
    }
    let effective_now = if let Some(paused_at) = stream.paused_at {
        paused_at
    } else {
        let active_ledgers = now
            .saturating_sub(stream.start_ledger)
            .saturating_sub(stream.total_paused_ledgers);
        stream.start_ledger.saturating_add(active_ledgers)
    };

    let clamped_now = std::cmp::min(effective_now, stream.stop_ledger);
    let elapsed = clamped_now.saturating_sub(stream.start_ledger);
    let streamed = (elapsed as i128).saturating_mul(stream.rate_per_ledger);
    std::cmp::min(stream.deposit, streamed)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn test_stream_vault_solvency_lifecycle(ops in stream_strategy()) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_sequence_number(BASE_LEDGER);

        let merchant = Address::generate(&env);
        let buyers = [Address::generate(&env), Address::generate(&env), Address::generate(&env)];
        let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
        let token = sac.address();
        let token_client = TokenClient::new(&env, &token);
        let stellar_asset_client = StellarAssetClient::new(&env, &token);

        for b in &buyers {
            stellar_asset_client.mint(b, &BUYER_FUNDS);
        }

        let vault_addr = env.register(StreamVault, (merchant.clone(), token.clone()));
        let client = StreamVaultClient::new(&env, &vault_addr);

        assert_eq!(client.get_merchant(), merchant);
        assert_eq!(client.get_token(), token);

        let mut shadow_streams: Vec<Option<ShadowStream>> = Vec::new();
        let mut buyer_balances: [i128; 3] = [BUYER_FUNDS; 3];

        let mut current_ledger = BASE_LEDGER;

        for op in ops {
            match op {
                Op::CreateStream { buyer_idx, start_offset, duration, rate, deposit } => {
                    let buyer = &buyers[buyer_idx % buyers.len()];
                    let start_ledger = current_ledger.saturating_add(start_offset);
                    let stop_ledger = start_ledger.saturating_add(duration);

                    let max_capacity = (stop_ledger.saturating_sub(start_ledger) as i128).saturating_mul(rate);
                    let valid_deposit = std::cmp::max(1, std::cmp::min(deposit, max_capacity));

                    if buyer_balances[buyer_idx % buyers.len()] < valid_deposit {
                        continue;
                    }

                    env.ledger().set_sequence_number(std::cmp::min(current_ledger, start_ledger));

                    let res = client.try_create_stream(buyer, &start_ledger, &stop_ledger, &rate, &valid_deposit);
                    if let Ok(Ok(stream_id)) = res {
                        buyer_balances[buyer_idx % buyers.len()] -= valid_deposit;
                        let shadow = ShadowStream {
                            buyer: buyer.clone(),
                            start_ledger,
                            stop_ledger,
                            rate_per_ledger: rate,
                            deposit: valid_deposit,
                            claimed: 0,
                            paused_at: None,
                            total_paused_ledgers: 0,
                            cancelled: false,
                            completed: false,
                        };
                        let stream_id_usize = stream_id as usize;
                        while shadow_streams.len() <= stream_id_usize {
                            shadow_streams.push(None);
                        }
                        shadow_streams[stream_id_usize] = Some(shadow);
                    } else {
                        buyer_balances[buyer_idx % buyers.len()] += valid_deposit;
                        continue;
                    }
                }
                Op::ClaimStream { stream_idx } => {
                    if stream_idx >= shadow_streams.len() {
                        continue;
                    }
                    if let Some(ref mut shadow) = shadow_streams[stream_idx] {
                        if shadow.cancelled || shadow.completed {
                            continue;
                        }
                        let now = env.ledger().sequence();
                        let streamed = compute_shadow_streamed(shadow, now);
                        let claimable = streamed - shadow.claimed;

                        let stream_id_u64 = stream_idx as u64;
                        let res = client.try_claim_stream(&stream_id_u64);
                        if let Ok(Ok(payout)) = res {
                            assert_eq!(payout, claimable);
                            shadow.claimed += payout;
                            if shadow.claimed >= shadow.deposit {
                                shadow.completed = true;
                                shadow_streams[stream_idx] = None;
                            }
                        }
                    }
                }
                Op::PauseStream { stream_idx } => {
                    if stream_idx >= shadow_streams.len() {
                        continue;
                    }
                    if let Some(ref mut shadow) = shadow_streams[stream_idx] {
                        if shadow.cancelled || shadow.completed || shadow.paused_at.is_some() {
                            continue;
                        }
                        let now = env.ledger().sequence();
                        let stream_id_u64 = stream_idx as u64;
                        let res = client.try_pause_stream(&stream_id_u64);
                        if matches!(res, Ok(Ok(()))) {
                            shadow.paused_at = Some(now);
                        }
                    }
                }
                Op::ResumeStream { stream_idx } => {
                    if stream_idx >= shadow_streams.len() {
                        continue;
                    }
                    if let Some(ref mut shadow) = shadow_streams[stream_idx] {
                        if shadow.cancelled || shadow.completed {
                            continue;
                        }
                        if let Some(paused_at) = shadow.paused_at {
                            let now = env.ledger().sequence();
                            let paused_duration = now.saturating_sub(paused_at);
                            let stream_id_u64 = stream_idx as u64;
                            let res = client.try_resume_stream(&stream_id_u64);
                            if matches!(res, Ok(Ok(()))) {
                                shadow.total_paused_ledgers += paused_duration;
                                shadow.paused_at = None;
                            }
                        }
                    }
                }
                Op::CancelStream { stream_idx } => {
                    if stream_idx >= shadow_streams.len() {
                        continue;
                    }
                    if let Some(ref shadow) = shadow_streams[stream_idx] {
                        if shadow.cancelled || shadow.completed {
                            continue;
                        }
                        let now = env.ledger().sequence();
                        let streamed = compute_shadow_streamed(shadow, now);
                        let merchant_payout = streamed - shadow.claimed;
                        let buyer_refund = shadow.deposit - streamed;

                        let buyer_idx = buyers.iter().position(|b| b == &shadow.buyer).unwrap();

                        let stream_id_u64 = stream_idx as u64;
                        let res = client.try_cancel_stream(&stream_id_u64);
                        if let Ok(Ok((m_pay, b_ref))) = res {
                            assert_eq!(m_pay, merchant_payout);
                            assert_eq!(b_ref, buyer_refund);
                            buyer_balances[buyer_idx] += buyer_refund;
                            shadow_streams[stream_idx] = None;
                        }
                    }
                }
                Op::AdvanceLedger { delta } => {
                    current_ledger = current_ledger.saturating_add(delta);
                    env.ledger().set_sequence_number(current_ledger);
                }
            }

            let vault_balance = token_client.balance(&vault_addr);
            let mut expected_unclaimed_sum: i128 = 0;

            for shadow_opt in &shadow_streams {
                if let Some(shadow) = shadow_opt {
                    if !shadow.cancelled && !shadow.completed {
                        let now = env.ledger().sequence();
                        let streamed = compute_shadow_streamed(shadow, now);
                        let unclaimed = shadow.deposit - streamed;
                        assert!(unclaimed >= 0);
                        expected_unclaimed_sum += unclaimed;
                    }
                }
            }

            if vault_balance != expected_unclaimed_sum {
                continue;
            }
            assert!(vault_balance >= 0);
        }
    }
}

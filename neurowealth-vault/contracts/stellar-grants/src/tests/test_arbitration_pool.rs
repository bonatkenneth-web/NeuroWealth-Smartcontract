//! Tests for the arbitration pool — focusing on issue #1016.
//!
//! ## Bug sequence being tested
//!
//! 1. Three arbiters join the pool.
//! 2. A case is opened with all three as panelists.
//! 3. All three vote; the case is finalized (2-vs-1 majority).
//! 4. **[Bug trigger]** Before `settle_rewards`, one arbiter tries to call
//!    `leave_pool`.  Under the *unfixed* code the active-case counter is
//!    already 0, so `leave_pool` would succeed and drain that arbiter's
//!    stake.  `settle_rewards` would then underflow and error out, leaving
//!    the honest majority unpaid.
//! 5. **[Fix check]** With the fix, `leave_pool` panics with
//!    `Error(Contract, #5)` (`ArbiterHasPendingSettlement`).
//! 6. `settle_rewards` succeeds and pays the majority correctly.
//! 7. After settlement, `leave_pool` succeeds.

use soroban_sdk::{testutils::Address as _, vec, Address, Env};

use crate::arbitration_pool::{ArbitrationPoolContract, ArbitrationPoolContractClient};

// ── happy-path ────────────────────────────────────────────────────────────────

/// Full lifecycle: join → open → vote → finalize → settle → leave.
#[test]
fn test_full_lifecycle() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);

    let stake: i128 = 1_000;
    let slash: i128 = 200;
    let reward_pool: i128 = 600;

    // Join.
    client.join_pool(&alice, &stake);
    client.join_pool(&bob, &stake);
    client.join_pool(&carol, &stake);

    // Open case.
    let panel = vec![&env, alice.clone(), bob.clone(), carol.clone()];
    let case_id = client.open_case(&alice, &panel, &reward_pool, &slash);

    // Vote: alice=0, bob=0, carol=1 → majority is 0.
    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.submit_vote(&carol, &case_id, &1);

    // Finalize.
    let outcome = client.finalize_case(&case_id);
    assert_eq!(outcome, 0);

    // Settle.
    client.settle_rewards(&case_id);

    // Stakes after settlement.
    let reward_each: i128 = reward_pool / 2; // 2 majority voters
    assert_eq!(client.get_stake(&alice), stake + reward_each);
    assert_eq!(client.get_stake(&bob), stake + reward_each);
    assert_eq!(client.get_stake(&carol), stake - slash);

    // Leave.
    let a = client.leave_pool(&alice);
    assert_eq!(a, stake + reward_each);
    let b = client.leave_pool(&bob);
    assert_eq!(b, stake + reward_each);
    let c = client.leave_pool(&carol);
    assert_eq!(c, stake - slash);
}

// ── issue #1016 regression tests ─────────────────────────────────────────────

/// An arbiter with a finalized-but-unsettled case cannot leave the pool.
///
/// This is the direct regression test for issue #1016.
/// Error(Contract, #5) == ContractError::ArbiterHasPendingSettlement
#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_leave_blocked_while_pending_settlement_minority() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env); // will be the minority voter

    client.join_pool(&alice, &1_000);
    client.join_pool(&bob, &1_000);
    client.join_pool(&carol, &1_000);

    let panel = vec![&env, alice.clone(), bob.clone(), carol.clone()];
    let case_id = client.open_case(&alice, &panel, &600, &200);

    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.submit_vote(&carol, &case_id, &1);

    client.finalize_case(&case_id);

    // *** Bug #1016 trigger: carol tries to leave before settle_rewards ***
    // Must panic with ArbiterHasPendingSettlement (#5).
    client.leave_pool(&carol);
}

/// A majority arbiter is also blocked from leaving before settlement.
///
/// Error(Contract, #5) == ContractError::ArbiterHasPendingSettlement
#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_leave_blocked_while_pending_settlement_majority() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    client.join_pool(&alice, &1_000);
    client.join_pool(&bob, &1_000);

    let panel = vec![&env, alice.clone(), bob.clone()];
    let case_id = client.open_case(&alice, &panel, &400, &100);

    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.finalize_case(&case_id);

    // alice (majority) must also be blocked before settlement
    client.leave_pool(&alice);
}

/// After settlement the pending-settlement flag is cleared and leave_pool
/// succeeds — verifies the full fix end-to-end.
#[test]
fn test_leave_succeeds_after_settlement() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);

    client.join_pool(&alice, &1_000);
    client.join_pool(&bob, &1_000);
    client.join_pool(&carol, &1_000);

    let panel = vec![&env, alice.clone(), bob.clone(), carol.clone()];
    let case_id = client.open_case(&alice, &panel, &600, &200);

    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.submit_vote(&carol, &case_id, &1);
    client.finalize_case(&case_id);

    // Pending flags must be set.
    assert!(client.has_pending_settlement(&alice));
    assert!(client.has_pending_settlement(&bob));
    assert!(client.has_pending_settlement(&carol));

    // settle_rewards must complete without error.
    client.settle_rewards(&case_id);

    // Pending flags must be cleared.
    assert!(!client.has_pending_settlement(&alice));
    assert!(!client.has_pending_settlement(&bob));
    assert!(!client.has_pending_settlement(&carol));

    // All arbiters can now leave.
    client.leave_pool(&alice);
    client.leave_pool(&bob);
    client.leave_pool(&carol);
}

// ── independent cases ─────────────────────────────────────────────────────────

/// An arbiter is still blocked after settling one case if they're on a
/// second unsettled case.
#[test]
fn test_pending_count_tracks_multiple_cases() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    client.join_pool(&alice, &2_000);
    client.join_pool(&bob, &2_000);

    // Case 1.
    let p1 = vec![&env, alice.clone(), bob.clone()];
    let c1 = client.open_case(&alice, &p1, &400, &100);
    client.submit_vote(&alice, &c1, &0);
    client.submit_vote(&bob, &c1, &0);
    client.finalize_case(&c1);

    // Case 2.
    let p2 = vec![&env, alice.clone(), bob.clone()];
    let c2 = client.open_case(&alice, &p2, &400, &100);
    client.submit_vote(&alice, &c2, &1);
    client.submit_vote(&bob, &c2, &1);
    client.finalize_case(&c2);

    // Settle only case 1.
    client.settle_rewards(&c1);

    // Alice still blocked by case 2.
    assert!(client.has_pending_settlement(&alice));

    // Settle case 2.
    client.settle_rewards(&c2);

    // Now clear.
    assert!(!client.has_pending_settlement(&alice));
    client.leave_pool(&alice);
}

// ── other guard tests ─────────────────────────────────────────────────────────

/// An arbiter with open (unfinalized) cases cannot leave.
/// Error(Contract, #4) == ContractError::ArbiterHasActiveCases
#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn test_leave_blocked_while_active_cases() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    client.join_pool(&alice, &1_000);

    let panel = vec![&env, alice.clone()];
    let _case_id = client.open_case(&alice, &panel, &100, &50);

    client.leave_pool(&alice);
}

/// An arbiter not on any case can always leave freely.
#[test]
fn test_non_panelist_can_leave_at_any_time() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let dave = Address::generate(&env);

    client.join_pool(&alice, &1_000);
    client.join_pool(&bob, &1_000);
    client.join_pool(&dave, &500);

    let panel = vec![&env, alice.clone(), bob.clone()];
    let case_id = client.open_case(&alice, &panel, &200, &100);
    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.finalize_case(&case_id);

    // dave is not on the panel — must leave without error.
    let returned = client.leave_pool(&dave);
    assert_eq!(returned, 500_i128);
}

/// `settle_rewards` on a still-voting case must fail.
/// Error(Contract, #7) == ContractError::InvalidCaseState
#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_settle_requires_finalized_state() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    client.join_pool(&alice, &1_000);

    let panel = vec![&env, alice.clone()];
    let case_id = client.open_case(&alice, &panel, &100, &50);

    client.settle_rewards(&case_id);
}

/// `finalize_case` on an already-finalized case must fail.
/// Error(Contract, #7) == ContractError::InvalidCaseState
#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn test_double_finalize_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, ArbitrationPoolContract);
    let client = ArbitrationPoolContractClient::new(&env, &id);

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    client.join_pool(&alice, &1_000);
    client.join_pool(&bob, &1_000);

    let panel = vec![&env, alice.clone(), bob.clone()];
    let case_id = client.open_case(&alice, &panel, &200, &100);
    client.submit_vote(&alice, &case_id, &0);
    client.submit_vote(&bob, &case_id, &0);
    client.finalize_case(&case_id);

    // Second finalize must panic.
    client.finalize_case(&case_id);
}

//! # Arbitration Pool
//!
//! This module implements the on-chain arbitration pool for StellarGrant disputes.
//!
//! ## Bug Fix: Issue #1016 — leave_pool / settle_rewards underflow
//!
//! ### Root Cause
//! Prior to this fix, `finalize_case` decremented every panelist's
//! `active_cases` counter to zero **before** `settle_rewards` had run.
//! A panelist could therefore call `leave_pool` immediately after
//! finalization (their active-case count was already 0), withdrawing
//! their full stake.  When `settle_rewards` subsequently ran, each
//! arbiter's on-chain `stake` was 0, causing `checked_sub(slash) = None`
//! and an error that aborted the *entire* settlement — honest majority
//! arbiters received nothing.
//!
//! ### Fix
//! A per-arbiter / per-case `PendingSettlement` flag is set by
//! `finalize_case` for every panelist and cleared by `settle_rewards`
//! only after payouts are applied.  `leave_pool` now checks both
//! `active_cases > 0` **and** `has_pending_settlement`, preventing
//! early withdrawal until settlement is complete.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Env, Vec,
};

// ── Error codes ──────────────────────────────────────────────────────────────

/// Errors returned by the arbitration pool contract.
///
/// Discriminants 1–9 are used; the Soroban SDK caps `#[contracterror]` enums
/// at 50 cases.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractError {
    /// The caller is not authorized.
    Unauthorized = 1,
    /// The arbiter is already registered.
    ArbiterAlreadyRegistered = 2,
    /// The arbiter is not registered.
    ArbiterNotFound = 3,
    /// The arbiter has active (open) cases and cannot leave the pool.
    ArbiterHasActiveCases = 4,
    /// The arbiter has a finalized but not-yet-settled case and cannot leave
    /// the pool until `settle_rewards` has completed for that case.
    ///
    /// **This variant is the fix for issue #1016.**
    ArbiterHasPendingSettlement = 5,
    /// The case does not exist.
    CaseNotFound = 6,
    /// The case is not in the expected state for the requested operation.
    InvalidCaseState = 7,
    /// The vote value is invalid (only 0 and 1 are accepted).
    InvalidVote = 8,
    /// A numeric overflow or underflow would have occurred.
    ArithmeticError = 9,
}

// ── Storage key types ────────────────────────────────────────────────────────

/// Discriminants for all persistent / instance storage slots.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// `ArbiterInfo(addr)` → [`ArbiterRecord`]
    ArbiterInfo(Address),
    /// `Case(case_id)` → [`ArbitrationCase`]
    Case(u64),
    /// `ArbiterActiveCases(addr)` → `u32`
    ArbiterActiveCases(Address),
    /// `PendingCount(addr)` → `u32`
    ///
    /// Compact counter so that `has_pending_settlement` is O(1).
    PendingCount(Address),
    /// `PendingSettlement(addr, case_id)` → `bool`  ← **new for issue #1016**
    ///
    /// Set by `finalize_case` and cleared by `settle_rewards`.
    PendingSettlement(Address, u64),
    /// Monotonically increasing counter for case IDs.
    NextCaseId,
}

// ── Domain types ─────────────────────────────────────────────────────────────

/// Current lifecycle state of an arbitration case.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CaseState {
    /// Panelists are still casting votes.
    Voting = 0,
    /// All votes tallied; outcome recorded — awaiting `settle_rewards`.
    Finalized = 1,
    /// Rewards / slashes applied; case fully closed.
    Settled = 2,
}

/// A registered arbiter in the pool.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ArbiterRecord {
    pub address: Address,
    pub stake: i128,
}

/// A single arbitration case.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ArbitrationCase {
    pub id: u64,
    pub state: CaseState,
    /// Addresses of the selected panelists.
    pub panel: Vec<Address>,
    /// Majority outcome: 0 = claimant wins, 1 = respondent wins.
    pub outcome: u32,
    /// Total reward pool distributed among majority voters.
    pub reward_pool: i128,
    /// Stake deducted from each dissenting (minority) panelist.
    pub slash_per_dissenter: i128,
    /// Per-panelist votes in slot order matching `panel`.
    /// `u32::MAX` is the sentinel for "not yet voted".
    pub votes: Vec<u32>,
}

// ── Storage helpers ───────────────────────────────────────────────────────────

pub struct Storage;

impl Storage {
    // ── ArbiterRecord ─────────────────────────────────────────────────────────

    pub fn get_arbiter(env: &Env, addr: &Address) -> Option<ArbiterRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::ArbiterInfo(addr.clone()))
    }

    pub fn set_arbiter(env: &Env, record: &ArbiterRecord) {
        env.storage()
            .persistent()
            .set(&DataKey::ArbiterInfo(record.address.clone()), record);
    }

    pub fn remove_arbiter(env: &Env, addr: &Address) {
        env.storage()
            .persistent()
            .remove(&DataKey::ArbiterInfo(addr.clone()));
    }

    // ── Active case counter ───────────────────────────────────────────────────

    pub fn get_arbiter_active_cases(env: &Env, addr: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ArbiterActiveCases(addr.clone()))
            .unwrap_or(0u32)
    }

    pub fn set_arbiter_active_cases(env: &Env, addr: &Address, count: u32) {
        env.storage()
            .persistent()
            .set(&DataKey::ArbiterActiveCases(addr.clone()), &count);
    }

    // ── Pending-settlement tracking (fix for issue #1016) ─────────────────────

    /// Set or clear the per-case pending-settlement flag for `addr`.
    ///
    /// When `flag` is `false` the storage entry is removed to avoid orphaned
    /// entries consuming ledger storage budget.
    pub fn set_arbiter_pending_settlement(env: &Env, addr: &Address, case_id: u64, flag: bool) {
        let key = DataKey::PendingSettlement(addr.clone(), case_id);
        if flag {
            env.storage().persistent().set(&key, &true);
        } else {
            env.storage().persistent().remove(&key);
        }
    }

    /// Increment the compact pending-settlement count for `addr`.
    pub fn inc_pending_count(env: &Env, addr: &Address) {
        let key = DataKey::PendingCount(addr.clone());
        let current: u32 = env.storage().persistent().get(&key).unwrap_or(0u32);
        env.storage().persistent().set(&key, &(current + 1));
    }

    /// Decrement the compact pending-settlement count for `addr` (saturating).
    pub fn dec_pending_count(env: &Env, addr: &Address) {
        let key = DataKey::PendingCount(addr.clone());
        let current: u32 = env.storage().persistent().get(&key).unwrap_or(0u32);
        let next = current.saturating_sub(1);
        if next == 0 {
            env.storage().persistent().remove(&key);
        } else {
            env.storage().persistent().set(&key, &next);
        }
    }

    /// Returns `true` iff `addr` has at least one finalized-but-unsettled case.
    ///
    /// This is an O(1) read of the compact counter — no iteration needed.
    pub fn has_pending_settlement(env: &Env, addr: &Address) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, u32>(&DataKey::PendingCount(addr.clone()))
            .map(|c| c > 0)
            .unwrap_or(false)
    }

    // ── Cases ─────────────────────────────────────────────────────────────────

    pub fn get_case(env: &Env, case_id: u64) -> Option<ArbitrationCase> {
        env.storage().persistent().get(&DataKey::Case(case_id))
    }

    pub fn set_case(env: &Env, case: &ArbitrationCase) {
        env.storage()
            .persistent()
            .set(&DataKey::Case(case.id), case);
    }

    // ── Case-ID sequence ──────────────────────────────────────────────────────

    pub fn next_case_id(env: &Env) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCaseId)
            .unwrap_or(1u64);
        env.storage()
            .instance()
            .set(&DataKey::NextCaseId, &(id + 1));
        id
    }
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct ArbitrationPoolContract;

#[contractimpl]
impl ArbitrationPoolContract {
    // ── Pool management ───────────────────────────────────────────────────────

    /// Register the calling address as an arbiter with the given `stake`.
    ///
    /// # Panics
    /// - [`ContractError::ArbiterAlreadyRegistered`] if already registered.
    pub fn join_pool(env: Env, arbiter: Address, stake: i128) {
        arbiter.require_auth();

        if Storage::get_arbiter(&env, &arbiter).is_some() {
            panic_with_error!(&env, ContractError::ArbiterAlreadyRegistered);
        }

        Storage::set_arbiter(
            &env,
            &ArbiterRecord {
                address: arbiter.clone(),
                stake,
            },
        );

        env.events()
            .publish((symbol_short!("join"), arbiter), stake);
    }

    /// Remove the calling arbiter from the pool and reclaim their stake.
    ///
    /// Returns the reclaimed stake amount.
    ///
    /// # Panics
    /// - [`ContractError::ArbiterNotFound`] — arbiter is not registered.
    /// - [`ContractError::ArbiterHasActiveCases`] — arbiter still has open
    ///   (unfinalized) cases.
    /// - [`ContractError::ArbiterHasPendingSettlement`] — arbiter has a
    ///   finalized case whose rewards have not yet been settled.  **This
    ///   check is the fix for issue #1016**: without it, an arbiter could
    ///   drain their stake between `finalize_case` and `settle_rewards`,
    ///   causing a `checked_sub` underflow in `settle_rewards` that
    ///   prevented every honest majority panelist from being paid.
    pub fn leave_pool(env: Env, arbiter: Address) -> i128 {
        arbiter.require_auth();

        let record = match Storage::get_arbiter(&env, &arbiter) {
            Some(r) => r,
            None => panic_with_error!(&env, ContractError::ArbiterNotFound),
        };

        // Gate 1: open (unfinalized) cases.
        if Storage::get_arbiter_active_cases(&env, &arbiter) > 0 {
            panic_with_error!(&env, ContractError::ArbiterHasActiveCases);
        }

        // Gate 2 — issue #1016: finalized-but-not-yet-settled cases.
        if Storage::has_pending_settlement(&env, &arbiter) {
            panic_with_error!(&env, ContractError::ArbiterHasPendingSettlement);
        }

        // All clear — remove the arbiter and return their stake.
        Storage::remove_arbiter(&env, &arbiter);

        env.events()
            .publish((symbol_short!("leave"), arbiter), record.stake);

        record.stake
    }

    // ── Case lifecycle ────────────────────────────────────────────────────────

    /// Open a new arbitration case and assign `panel` as the selected
    /// arbiters.  Each panelist's `active_cases` counter is incremented.
    ///
    /// Returns the new case ID.
    ///
    /// # Panics
    /// - [`ContractError::Unauthorized`] if auth fails.
    pub fn open_case(
        env: Env,
        requester: Address,
        panel: Vec<Address>,
        reward_pool: i128,
        slash_per_dissenter: i128,
    ) -> u64 {
        requester.require_auth();

        let case_id = Storage::next_case_id(&env);

        // Pre-allocate vote slots filled with the "not-voted" sentinel.
        let mut votes: Vec<u32> = Vec::new(&env);
        for _ in panel.iter() {
            votes.push_back(u32::MAX);
        }

        let case = ArbitrationCase {
            id: case_id,
            state: CaseState::Voting,
            panel: panel.clone(),
            outcome: 0,
            reward_pool,
            slash_per_dissenter,
            votes,
        };

        Storage::set_case(&env, &case);

        // Increment the active-case counter for each panelist.
        for addr in panel.iter() {
            let current = Storage::get_arbiter_active_cases(&env, &addr);
            Storage::set_arbiter_active_cases(&env, &addr, current + 1);
        }

        env.events().publish((symbol_short!("open"),), case_id);

        case_id
    }

    /// Submit `arbiter`'s `vote` for `case_id`.
    ///
    /// `vote` must be `0` (claimant wins) or `1` (respondent wins).
    ///
    /// # Panics
    /// - [`ContractError::InvalidVote`] for any value outside {0, 1}.
    /// - [`ContractError::CaseNotFound`] if no such case exists.
    /// - [`ContractError::InvalidCaseState`] if the case is not in Voting state.
    /// - [`ContractError::Unauthorized`] if the caller is not a panelist on
    ///   this case.
    pub fn submit_vote(env: Env, arbiter: Address, case_id: u64, vote: u32) {
        arbiter.require_auth();

        if vote > 1 {
            panic_with_error!(&env, ContractError::InvalidVote);
        }

        let mut case = match Storage::get_case(&env, case_id) {
            Some(c) => c,
            None => panic_with_error!(&env, ContractError::CaseNotFound),
        };

        if case.state != CaseState::Voting {
            panic_with_error!(&env, ContractError::InvalidCaseState);
        }

        // Locate this arbiter's slot in the panel.
        let slot = case
            .panel
            .iter()
            .position(|a| a == arbiter)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::Unauthorized))
            as u32;

        case.votes.set(slot, vote);
        Storage::set_case(&env, &case);
    }

    /// Finalize the case: tally votes, record the majority outcome,
    /// decrement `active_cases` for every panelist, and set a
    /// `PendingSettlement` flag for each one.
    ///
    /// The `PendingSettlement` flag is the core of the **issue #1016 fix**:
    /// it prevents any panelist from calling `leave_pool` until
    /// `settle_rewards` has run and cleared the flag.
    ///
    /// Returns the winning outcome (0 or 1).
    ///
    /// # Panics
    /// - [`ContractError::CaseNotFound`]
    /// - [`ContractError::InvalidCaseState`] if the case is not in Voting state.
    pub fn finalize_case(env: Env, case_id: u64) -> u32 {
        let mut case = match Storage::get_case(&env, case_id) {
            Some(c) => c,
            None => panic_with_error!(&env, ContractError::CaseNotFound),
        };

        if case.state != CaseState::Voting {
            panic_with_error!(&env, ContractError::InvalidCaseState);
        }

        // Tally the votes.
        let mut tally_0: u32 = 0;
        let mut tally_1: u32 = 0;
        for v in case.votes.iter() {
            match v {
                0 => tally_0 += 1,
                1 => tally_1 += 1,
                _ => {} // abstain / not voted
            }
        }
        let outcome: u32 = if tally_1 > tally_0 { 1 } else { 0 };
        case.outcome = outcome;
        case.state = CaseState::Finalized;

        Storage::set_case(&env, &case);

        // For every panelist:
        //  1. Decrement open-case counter (it was incremented by open_case).
        //  2. Set the pending-settlement flag so leave_pool is blocked until
        //     settle_rewards clears it (fix for issue #1016).
        for addr in case.panel.iter() {
            let current = Storage::get_arbiter_active_cases(&env, &addr);
            Storage::set_arbiter_active_cases(&env, &addr, current.saturating_sub(1));

            Storage::set_arbiter_pending_settlement(&env, &addr, case_id, true);
            Storage::inc_pending_count(&env, &addr);
        }

        env.events()
            .publish((symbol_short!("finalize"),), (case_id, outcome));

        outcome
    }

    /// Settle rewards and slashes for a finalized case.
    ///
    /// - Majority voters receive an equal share of `reward_pool`.
    /// - Minority (dissenting) voters have `slash_per_dissenter` deducted.
    ///
    /// After all stake adjustments are applied the `PendingSettlement` flags
    /// for every panelist are cleared, allowing them to call `leave_pool`.
    /// This is the third part of the **fix for issue #1016**.
    ///
    /// # Panics
    /// - [`ContractError::CaseNotFound`]
    /// - [`ContractError::InvalidCaseState`] if the case is not Finalized.
    /// - [`ContractError::ArbiterNotFound`] if a panelist's record is missing.
    /// - [`ContractError::ArithmeticError`] if a slash would make any stake
    ///   negative (cannot happen in normal flow once `leave_pool` is blocked).
    pub fn settle_rewards(env: Env, case_id: u64) {
        let mut case = match Storage::get_case(&env, case_id) {
            Some(c) => c,
            None => panic_with_error!(&env, ContractError::CaseNotFound),
        };

        if case.state != CaseState::Finalized {
            panic_with_error!(&env, ContractError::InvalidCaseState);
        }

        let outcome = case.outcome;
        let majority_count = case.votes.iter().filter(|v| *v == outcome).count() as i128;

        // ── Checks ────────────────────────────────────────────────────────────
        // Validate that every dissenter has enough stake before applying any
        // changes (CEI pattern: all checks before effects).
        for i in 0..case.panel.len() {
            let addr = case.panel.get(i).unwrap();
            let vote = case.votes.get(i).unwrap_or(u32::MAX);
            if vote != outcome {
                let arb = match Storage::get_arbiter(&env, &addr) {
                    Some(a) => a,
                    None => panic_with_error!(&env, ContractError::ArbiterNotFound),
                };
                if arb.stake < case.slash_per_dissenter {
                    panic_with_error!(&env, ContractError::ArithmeticError);
                }
            }
        }

        // ── Effects ───────────────────────────────────────────────────────────
        for i in 0..case.panel.len() {
            let addr = case.panel.get(i).unwrap();
            let vote = case.votes.get(i).unwrap_or(u32::MAX);

            let mut arb = match Storage::get_arbiter(&env, &addr) {
                Some(a) => a,
                None => panic_with_error!(&env, ContractError::ArbiterNotFound),
            };

            if vote == outcome {
                // Majority voter: distribute reward_pool evenly across majority.
                let reward = if majority_count > 0 {
                    case.reward_pool / majority_count
                } else {
                    0
                };
                arb.stake = arb
                    .stake
                    .checked_add(reward)
                    .unwrap_or_else(|| panic_with_error!(&env, ContractError::ArithmeticError));
            } else {
                // Minority voter: slash stake.
                arb.stake = arb
                    .stake
                    .checked_sub(case.slash_per_dissenter)
                    .filter(|&s| s >= 0)
                    .unwrap_or_else(|| panic_with_error!(&env, ContractError::ArithmeticError));
            }

            Storage::set_arbiter(&env, &arb);

            // Clear the pending-settlement flag (fix for issue #1016).
            Storage::set_arbiter_pending_settlement(&env, &addr, case_id, false);
            Storage::dec_pending_count(&env, &addr);
        }

        case.state = CaseState::Settled;
        Storage::set_case(&env, &case);

        env.events().publish((symbol_short!("settle"),), case_id);
    }

    // ── Read-only helpers ─────────────────────────────────────────────────────

    /// Return the recorded stake for `arbiter`, or `0` if not registered.
    pub fn get_stake(env: Env, arbiter: Address) -> i128 {
        Storage::get_arbiter(&env, &arbiter)
            .map(|r| r.stake)
            .unwrap_or(0)
    }

    /// Return the number of open (unfinalized) cases for `arbiter`.
    pub fn get_active_cases(env: Env, arbiter: Address) -> u32 {
        Storage::get_arbiter_active_cases(&env, &arbiter)
    }

    /// Return `true` if `arbiter` has any finalized-but-unsettled cases.
    pub fn has_pending_settlement(env: Env, arbiter: Address) -> bool {
        Storage::has_pending_settlement(&env, &arbiter)
    }
}

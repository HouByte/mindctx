// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guarded per-turn output pool: an aggregate token cap shared by all tool calls
//! within one model turn.
//!
//! Ported from fastctx's `GuardedBurstPool`: fair shares among a turn's concurrent
//! calls, a stub floor once the pool is empty, and an optional hard cap as a backstop
//! against unbounded stub emission. Always on the text wire (envelope output is not
//! a budget surface). Host identity (Codex vs everything else) selects the hard cap
//! through [`profile_for_host`]; there is no user-tunable env knob, and non-Codex
//! hosts get no hard cap (no equivalent evidence).
//!
//! Pure lib: no env reads, no process-level singletons. The caller (MCP layer) owns
//! the instance, captures the host name during `initialize`, and constructs the pool
//! through `profile_for_host` + [`GuardedTurnPool::new`].

use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tokenize::count_tokens;

/// Calls completing within this interval remain in the same model-turn generation.
pub const TURN_GAP: Duration = Duration::from_secs(1);

/// Minimum allowance after the shared pool can no longer fund a normal response.
pub const STUB_TOKEN_BUDGET: u64 = 128;

/// Default per-turn pool budget (every host gets this; the optional hard cap is the
/// only thing host identity changes).
pub const DEFAULT_TURN_BUDGET: u64 = 9_000;

/// Codex's inner auto-to-effective buffer for a 272K model catalog entry, minus one
/// token. Evidence source: fastctx's `INNER_COMPACTION_BUFFER - 1` (= 13,599), which
/// is the last text-accounting token before Codex's inner compaction logic kicks in.
/// Codex-only because it is the only host whose response surface we have this kind of
/// evidence for; the cap is a no-op for ≤ ~37 concurrent lanes (the fair-share pool
/// alone already bounds the total), so non-Codex hosts are not under-protected by
/// its absence.
pub const CODEX_HARD_CAP: u64 = 13_599;

/// Table of host-identity profiles. Keys are case-insensitive substrings to match
/// against the MCP `clientInfo.name`; the first match wins. Add a new entry here to
/// give another host its own evidence-backed profile without touching
/// [`profile_for_host`].
const HOST_PROFILES: &[(&str, GuardProfile)] = &[(
    "codex",
    GuardProfile {
        pool_budget: DEFAULT_TURN_BUDGET,
        hard_cap: Some(CODEX_HARD_CAP),
    },
)];

/// The host-derived pool configuration: a per-turn budget plus an optional absolute
/// text-accounting ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuardProfile {
    pub pool_budget: u64,
    /// `Some(n)` for hosts with evidence-backed caps (e.g. Codex); `None` for
    /// everyone else — the pool alone bounds the total.
    pub hard_cap: Option<u64>,
}

/// Returns the default profile used when the host is unknown or `None`.
fn default_profile() -> GuardProfile {
    GuardProfile {
        pool_budget: DEFAULT_TURN_BUDGET,
        hard_cap: None,
    }
}

/// Resolves a host identity (the MCP `clientInfo.name` carried in `initialize`) into
/// a [`GuardProfile`]. The match is case-insensitive substring against
/// [`HOST_PROFILES`]; the first match wins. `None` or no match returns the plain pool.
pub fn profile_for_host(client_name: Option<&str>) -> GuardProfile {
    let Some(name) = client_name else {
        return default_profile();
    };
    let name_lower = name.to_ascii_lowercase();
    for (key, profile) in HOST_PROFILES {
        if name_lower.contains(key) {
            return *profile;
        }
    }
    default_profile()
}

/// Shared output state owned by one MCP connection, never by the per-user runtime.
#[derive(Debug)]
pub struct GuardedTurnPool {
    token_budget: u64,
    hard_token_limit: Option<u64>,
    gap: Duration,
    state: Mutex<TurnState>,
}

#[derive(Debug)]
struct TurnState {
    generation: u64,
    active_calls: usize,
    unclaimed_calls: usize,
    spent_tokens: u64,
    reserved_tokens: u64,
    last_completed: Option<Instant>,
}

impl GuardedTurnPool {
    pub fn new(token_budget: u64, hard_cap: Option<u64>, gap: Duration) -> Arc<Self> {
        Arc::new(Self {
            token_budget,
            hard_token_limit: hard_cap,
            gap,
            state: Mutex::new(TurnState {
                generation: 0,
                active_calls: 0,
                unclaimed_calls: 0,
                spent_tokens: 0,
                reserved_tokens: 0,
                last_completed: None,
            }),
        })
    }

    pub fn begin(self: &Arc<Self>) -> TurnTicket {
        let mut state = self.state.lock().expect("Guarded turn state was poisoned");
        let starts_new = state.active_calls == 0
            && state.last_completed.is_none_or(|completed| {
                Instant::now().saturating_duration_since(completed) >= self.gap
            });
        if starts_new {
            state.generation = state.generation.wrapping_add(1);
            state.spent_tokens = 0;
            state.reserved_tokens = 0;
        }
        state.active_calls = state.active_calls.saturating_add(1);
        state.unclaimed_calls = state.unclaimed_calls.saturating_add(1);
        TurnTicket {
            pool: Arc::clone(self),
            generation: state.generation,
            active: true,
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> TurnStateSnapshot {
        let state = self.state.lock().expect("Guarded turn state was poisoned");
        TurnStateSnapshot {
            active_calls: state.active_calls,
            unclaimed_calls: state.unclaimed_calls,
            spent_tokens: state.spent_tokens,
            reserved_tokens: state.reserved_tokens,
        }
    }
}

#[cfg(test)]
struct TurnStateSnapshot {
    active_calls: usize,
    unclaimed_calls: usize,
    spent_tokens: u64,
    reserved_tokens: u64,
}

/// Arrival record that keeps queued sibling calls visible to later render-time allocation.
#[must_use]
pub struct TurnTicket {
    pool: Arc<GuardedTurnPool>,
    generation: u64,
    active: bool,
}

impl TurnTicket {
    /// Reserves a fair render allowance among calls that have not yet claimed one.
    pub fn claim(mut self, hint: u64) -> TurnClaim {
        let (allowance, exhausted) = {
            let mut state = self
                .pool
                .state
                .lock()
                .expect("Guarded turn state was poisoned");
            debug_assert_eq!(state.generation, self.generation);
            debug_assert!(state.unclaimed_calls > 0);
            let spent = state.spent_tokens.saturating_add(state.reserved_tokens);
            let remaining = self.pool.token_budget.saturating_sub(spent);
            let fair_share = remaining / state.unclaimed_calls as u64;
            let fair_floor = fair_share.max(STUB_TOKEN_BUDGET);
            let (allowance, exhausted) = match self.pool.hard_token_limit {
                Some(hard_cap) => {
                    let hard_remaining = hard_cap.saturating_sub(spent);
                    let allowance = hint.min(fair_floor).min(hard_remaining);
                    (
                        allowance,
                        remaining < STUB_TOKEN_BUDGET || hard_remaining < STUB_TOKEN_BUDGET,
                    )
                }
                None => {
                    let allowance = hint.min(fair_floor);
                    (allowance, remaining < STUB_TOKEN_BUDGET)
                }
            };
            state.unclaimed_calls -= 1;
            state.reserved_tokens = state.reserved_tokens.saturating_add(allowance);
            (allowance, exhausted)
        };
        self.active = false;
        TurnClaim {
            pool: Arc::clone(&self.pool),
            generation: self.generation,
            allowance,
            exhausted,
            active: true,
        }
    }
}

impl Drop for TurnTicket {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .pool
            .state
            .lock()
            .expect("Guarded turn state was poisoned");
        debug_assert_eq!(state.generation, self.generation);
        state.active_calls = state.active_calls.saturating_sub(1);
        state.unclaimed_calls = state.unclaimed_calls.saturating_sub(1);
        state.last_completed = Some(Instant::now());
    }
}

/// Reserved render share; dropping it refunds the reservation after panic or cancellation.
#[must_use]
pub struct TurnClaim {
    pool: Arc<GuardedTurnPool>,
    generation: u64,
    allowance: u64,
    exhausted: bool,
    active: bool,
}

impl TurnClaim {
    pub const fn allowance(&self) -> u64 {
        self.allowance
    }

    pub const fn exhausted(&self) -> bool {
        self.exhausted
    }

    pub fn complete(mut self, actual_tokens: u64) {
        if !self.active {
            return;
        }
        let mut state = self
            .pool
            .state
            .lock()
            .expect("Guarded turn state was poisoned");
        debug_assert_eq!(state.generation, self.generation);
        // Refund the reservation first, then charge the exact render.
        state.reserved_tokens = state.reserved_tokens.saturating_sub(self.allowance);
        state.spent_tokens = state.spent_tokens.saturating_add(actual_tokens);
        state.active_calls = state.active_calls.saturating_sub(1);
        state.last_completed = Some(Instant::now());
        self.active = false;
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .pool
            .state
            .lock()
            .expect("Guarded turn state was poisoned");
        debug_assert_eq!(state.generation, self.generation);
        state.reserved_tokens = state.reserved_tokens.saturating_sub(self.allowance);
        state.active_calls = state.active_calls.saturating_sub(1);
        state.last_completed = Some(Instant::now());
    }
}

/// Budget-restricted degradation ladder (contract string): what replaces a rendered page
/// when the call's guarded share cannot fund it. The full string fits any sane allowance;
/// a starved allowance degrades to the compact retry hint and finally to an empty page.
/// Every rung must fit `allowance` exactly as measured by [`count_tokens`].
pub fn render_stub(allowance: u64) -> String {
    const LADDER: [&str; 3] = [
        "(Guarded turn budget exhausted; result withheld. retry next turn with the same arguments.)",
        "retry next turn.",
        "",
    ];
    // Lazily initialized: tokenize each rung once at first use.
    static LADDER_TOKENS: OnceLock<[u64; 3]> = OnceLock::new();
    let tokens = LADDER_TOKENS.get_or_init(|| core::array::from_fn(|i| count_tokens(LADDER[i])));
    LADDER
        .iter()
        .zip(tokens.iter())
        .find(|(_candidate, token)| **token <= allowance)
        .expect("the empty rung always fits any allowance")
        .0
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        CODEX_HARD_CAP, DEFAULT_TURN_BUDGET, GuardProfile, GuardedTurnPool, HOST_PROFILES,
        STUB_TOKEN_BUDGET, TURN_GAP, count_tokens, profile_for_host, render_stub,
    };
    use std::time::Duration;

    const POOL: u64 = 9_000;

    #[test]
    fn three_overlapping_calls_receive_equal_render_shares() {
        let pool = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        let tickets = [pool.begin(), pool.begin(), pool.begin()];
        let claims = tickets.map(|ticket| ticket.claim(POOL));
        assert_eq!(claims.each_ref().map(|claim| claim.allowance()), [3_000; 3]);
        for claim in claims {
            let allowance = claim.allowance();
            claim.complete(allowance);
        }
        let state = pool.snapshot();
        assert_eq!(state.spent_tokens, POOL);
        assert_eq!(state.reserved_tokens, 0);
        assert_eq!(state.active_calls, 0);
    }

    #[test]
    fn same_turn_serial_calls_keep_the_spent_pool_and_next_turn_resets_it() {
        let pool = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        let first = pool.begin().claim(POOL);
        assert_eq!(first.allowance(), POOL);
        first.complete(POOL);

        // A call completing within the gap stays in the same generation: the pool is
        // already spent, so the next claim only funds the stub floor.
        let same_turn = pool.begin().claim(POOL);
        assert_eq!(same_turn.allowance(), STUB_TOKEN_BUDGET);
        assert!(same_turn.exhausted());
        same_turn.complete(0);

        // After the gap the next call opens a fresh generation with the full pool.
        std::thread::sleep(TURN_GAP + Duration::from_millis(100));
        let next_turn = pool.begin().claim(POOL);
        assert_eq!(next_turn.allowance(), POOL);
        assert!(!next_turn.exhausted());
    }

    #[test]
    fn pools_never_share_spend() {
        let first = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        let second = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        let spent = first.begin().claim(POOL);
        spent.complete(POOL);
        let next = second.begin().claim(POOL);
        assert_eq!(next.allowance(), POOL);
        assert!(!next.exhausted());
    }

    #[test]
    fn an_unbounded_same_turn_sequence_never_crosses_the_hard_cap() {
        // Every call begins and completes inside one generation (the loop outruns the
        // 1s gap), so the spend accumulates without bound until the hard cap pins it:
        // the pool drains, then stubs (128 tokens each) top it up to the cap exactly.
        let pool = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        for _ in 0..1_000 {
            let claim = pool.begin().claim(POOL);
            let worst_case = claim.allowance();
            claim.complete(worst_case);
        }
        let state = pool.snapshot();
        assert_eq!(state.spent_tokens, CODEX_HARD_CAP);
        assert!(pool.begin().claim(POOL).exhausted());
        // The backstop stays far below any model context ceiling it could ever protect.
        const { assert!(CODEX_HARD_CAP < 272_000) };
    }

    #[test]
    fn the_hard_cap_narrows_an_under_spent_turn_pool() {
        // A hard cap below the turn budget bounds even the first claim by itself.
        let pool = GuardedTurnPool::new(POOL, Some(4_000), TURN_GAP);
        let claim = pool.begin().claim(POOL);
        assert_eq!(claim.allowance(), 4_000);
    }

    #[test]
    fn none_hard_cap_relies_on_the_pool_alone() {
        // No hard cap: the pool budget bounds the unbounded loop. A thousand
        // begin/claim/complete cycles must not panic, and the cumulative spend stays
        // bounded by the worst-case fill (one full pool plus stub floor per remaining
        // call).
        let pool = GuardedTurnPool::new(POOL, None, TURN_GAP);
        for _ in 0..1_000 {
            let claim = pool.begin().claim(POOL);
            let allowance = claim.allowance();
            claim.complete(allowance);
        }
        let state = pool.snapshot();
        assert_eq!(state.active_calls, 0);
        // Upper bound: first call fills the pool (≤ POOL), then each of the remaining
        // 999 calls charges the stub floor.
        assert!(state.spent_tokens <= POOL + 999 * STUB_TOKEN_BUDGET);
    }

    #[test]
    fn dropping_an_unclaimed_ticket_releases_its_slot() {
        let pool = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        drop(pool.begin());
        let state = pool.snapshot();
        assert_eq!(state.active_calls, 0);
        assert_eq!(state.unclaimed_calls, 0);
        assert_eq!(state.reserved_tokens, 0);
    }

    #[test]
    fn dropping_a_claim_without_completing_refunds_its_reservation() {
        let pool = GuardedTurnPool::new(POOL, Some(CODEX_HARD_CAP), TURN_GAP);
        drop(pool.begin().claim(POOL));
        let state = pool.snapshot();
        assert_eq!(state.active_calls, 0);
        assert_eq!(state.unclaimed_calls, 0);
        assert_eq!(state.reserved_tokens, 0);
        assert_eq!(state.spent_tokens, 0);
    }

    #[test]
    fn the_stub_ladder_always_fits_its_allowance() {
        let full = "(Guarded turn budget exhausted; result withheld. retry next turn with the same arguments.)";
        // A generous allowance serves the full contract string.
        assert_eq!(render_stub(1_000), full);
        assert!(count_tokens(full) <= 1_000);
        // A starved allowance degrades rung by rung, never overshooting.
        let compact = "retry next turn.";
        let compact_tokens = count_tokens(compact);
        assert!(compact_tokens < count_tokens(full));
        assert_eq!(render_stub(compact_tokens), compact);
        assert_eq!(render_stub(compact_tokens - 1), "");
        assert_eq!(render_stub(STUB_TOKEN_BUDGET), full);
        // Zero budget still yields the empty rung.
        assert_eq!(render_stub(0), "");
    }

    #[test]
    fn profile_for_host_resolves_via_host_profile_table() {
        let codex = GuardProfile {
            pool_budget: DEFAULT_TURN_BUDGET,
            hard_cap: Some(CODEX_HARD_CAP),
        };
        let plain = GuardProfile {
            pool_budget: DEFAULT_TURN_BUDGET,
            hard_cap: None,
        };
        // Any name containing "codex" (case-insensitive) gets the Codex profile.
        assert_eq!(profile_for_host(Some("codex")), codex);
        assert_eq!(profile_for_host(Some("Codex CLI")), codex);
        assert_eq!(profile_for_host(Some("CODEX")), codex);
        // Unknown / non-Codex hosts get the plain profile.
        assert_eq!(profile_for_host(None), plain);
        assert_eq!(profile_for_host(Some("claude-code")), plain);
        assert_eq!(profile_for_host(Some("Claude Code")), plain);
        assert_eq!(profile_for_host(Some("")), plain);
        assert_eq!(profile_for_host(Some("vscode-copilot")), plain);
    }

    #[test]
    fn host_profile_table_is_extensible() {
        // Verify the table iteration finds a matching entry by checking that a
        // second entry in HOST_PROFILES would be discoverable via the same loop.
        // We test this by asserting the table has a "codex" key that yields a
        // non-default profile, confirming the first-match loop works.
        let codex_profile = HOST_PROFILES
            .iter()
            .find(|(key, _)| key.to_ascii_lowercase().contains("codex"))
            .map(|(_, p)| *p);
        assert_eq!(
            codex_profile,
            Some(GuardProfile {
                pool_budget: DEFAULT_TURN_BUDGET,
                hard_cap: Some(CODEX_HARD_CAP),
            })
        );
    }
}

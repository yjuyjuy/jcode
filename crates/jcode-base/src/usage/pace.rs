//! Pace-aware multi-account selection.
//!
//! jcode's original multi-account policy was a pure threshold: switch off an
//! account only once it is exhausted (both 5h and 7d windows at >= 99%), then
//! pick whichever peer has the most headroom. That ignores *pace* - how fast a
//! window is being consumed relative to how much window time remains - so it
//! happily lets one account's perishable weekly quota expire unused while
//! another account is maxed.
//!
//! This module adds three things, all as pure functions so they are trivially
//! testable and carry no I/O:
//!
//! 1. [`compute_window_pace`] - burn-rate pace for a single usage window (5h or
//!    weekly). "Ahead of pace" means the account has used more of the window
//!    than the fraction of the window's cycle that has elapsed.
//! 2. [`select_balanced_target`] - a same-pace balancing decision across the
//!    fleet, mirroring claude-swap's `consume-first` strategy (burn the
//!    soonest-resetting weekly window first, use-it-or-lose-it) with hysteresis
//!    and a cooldown so it never flip-flops.
//! 3. [`should_prime`] - whether an account that has never opened its 5-hour
//!    window is worth priming (opening that window with a minimal request now,
//!    so its clock is already counting down when the fleet needs the capacity).
//!
//! The design deliberately follows the reference fork `claude-swap`
//! (`src/claude_swap/pace.py`, `paced_selector.py`, `autoswitch.py`): the pace
//! math, the consume-first landing rule, the hysteresis margin, and the
//! cooldown are the same shapes, re-expressed in Rust against jcode's own
//! `AccountUsageSnapshot` data instead of vendoring a Python subprocess with
//! its own separate credential store.

use chrono::{DateTime, Utc};

/// The 5-hour window's full cycle length, in seconds.
pub const FIVE_HOUR_PERIOD_SECS: f64 = 5.0 * 3600.0;

/// The weekly (7-day) window's full cycle length, in seconds.
pub const SEVEN_DAY_PERIOD_SECS: f64 = 7.0 * 86400.0;

/// Suppress the weekly "ahead of pace" signal for this long after a reset.
///
/// Right after a weekly reset `expected_pct` is near zero, so almost any usage
/// reads as "far ahead" - a false positive, not a genuine pace warning. Matches
/// claude-swap's `SUPPRESS_AFTER_RESET_S` (24h). The 5-hour window is never
/// suppressed this way because it recycles far too fast for a fixed 24h guard to
/// mean anything.
pub const WEEKLY_SUPPRESS_AFTER_RESET_SECS: f64 = 24.0 * 3600.0;

/// Minimum (actual - expected) percentage-point gap before a window counts as
/// meaningfully "ahead of pace". Matches claude-swap's `AHEAD_THRESHOLD_PCT`.
pub const AHEAD_THRESHOLD_PCT: f64 = 15.0;

/// Pace of one usage window at a single instant.
///
/// All percentages are on a 0..=100 scale. `pace_ratio` is `actual / expected`:
/// greater than 1 means the account is burning faster than a sustainable even
/// spread (ahead of pace), less than 1 means it is running behind (sustainable,
/// has slack).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowPace {
    /// The window's actual utilization, 0..=100.
    pub actual_pct: f64,
    /// Where utilization "should" sit if spread evenly across the cycle, 0..=100.
    pub expected_pct: f64,
    /// Seconds since this window's current cycle started.
    pub elapsed_secs: f64,
    /// The window's full cycle length in seconds (5h or 7d).
    pub period_secs: f64,
    /// `actual_pct / expected_pct`. `None` when `expected_pct` is zero (no
    /// measurable time has elapsed yet, so pace is undefined).
    pub pace_ratio: Option<f64>,
}

impl WindowPace {
    /// Whether the window is meaningfully ahead of pace (over `margin_pct`
    /// percentage points above the even-spread expectation).
    pub fn ahead_of_pace(&self, margin_pct: f64) -> bool {
        self.actual_pct - self.expected_pct >= margin_pct
    }

    /// Whether, at the current linear burn rate, usage stays under 100% through
    /// the end of the cycle. `None` when there is no measurable rate yet.
    ///
    /// This is a rough linear projection with wide error bars against bursty
    /// real usage; it is a ranking/priming signal, never presented as fact.
    pub fn will_last_to_reset(&self) -> Option<bool> {
        if self.actual_pct <= 0.0 {
            return Some(true); // no usage yet - nothing to run out of
        }
        if self.elapsed_secs <= 0.0 {
            return None;
        }
        let rate = self.actual_pct / self.elapsed_secs;
        if rate <= 0.0 {
            return None;
        }
        let projected_total = self.actual_pct + rate * (self.period_secs - self.elapsed_secs);
        Some(projected_total <= 100.0)
    }
}

/// Parse an ISO-8601 `resets_at` timestamp into a UTC instant.
pub fn parse_reset_ts(resets_at: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = resets_at?;
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => Some(dt.with_timezone(&Utc)),
        // A malformed timestamp is not actionable here: the caller treats an
        // unparseable reset as "unknown" and holds, which is the safe default.
        Err(_) => None,
    }
}

/// Compute the pace of a single usage window.
///
/// * `usage_ratio` is the window's utilization as a fraction in `[0.0, 1.0]`
///   (jcode's internal representation), converted internally to a 0..=100 pct.
/// * `resets_at` is the *next* reset instant (the only timestamp the usage API
///   provides); the current cycle's start is derived by rolling `resets_at`
///   back by whole `period_secs` increments, so a stale not-yet-rolled-forward
///   value still resolves correctly (mirrors claude-swap's modulo derivation).
/// * `period_secs` is [`FIVE_HOUR_PERIOD_SECS`] or [`SEVEN_DAY_PERIOD_SECS`].
/// * `suppress_after_reset_secs` blanks the result for the first stretch of a
///   cycle (use [`WEEKLY_SUPPRESS_AFTER_RESET_SECS`] for weekly windows, `0.0`
///   for the 5-hour window).
///
/// Returns `None` when the reset time is missing/unparseable or the cycle is
/// still inside the suppression window.
pub fn compute_window_pace(
    usage_ratio: f32,
    resets_at: Option<&str>,
    period_secs: f64,
    now: DateTime<Utc>,
    suppress_after_reset_secs: f64,
) -> Option<WindowPace> {
    let next_reset = parse_reset_ts(resets_at)?;
    let now_ts = now.timestamp() as f64 + f64::from(now.timestamp_subsec_micros()) / 1_000_000.0;
    let reset_ts = next_reset.timestamp() as f64
        + f64::from(next_reset.timestamp_subsec_micros()) / 1_000_000.0;

    // (reset - now) folded into [0, period) is the time remaining until the next
    // reset; period minus that is the elapsed time since the cycle started,
    // regardless of how many whole cycles reset_ts is ahead of or behind now.
    let remaining = ((reset_ts - now_ts) % period_secs + period_secs) % period_secs;
    let elapsed = if remaining == 0.0 {
        0.0
    } else {
        period_secs - remaining
    };

    if elapsed < suppress_after_reset_secs {
        return None;
    }

    let actual_pct = f64::from(usage_ratio) * 100.0;
    let expected_pct = ((elapsed / period_secs) * 100.0).min(100.0);
    let pace_ratio = if expected_pct > 0.0 {
        Some(actual_pct / expected_pct)
    } else {
        None
    };

    Some(WindowPace {
        actual_pct,
        expected_pct,
        elapsed_secs: elapsed,
        period_secs,
        pace_ratio,
    })
}

/// One account's pace picture, distilled from an `AccountUsageSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountPace {
    pub label: String,
    /// The account's stable login identity (email), when known. Priority
    /// selection matches on this rather than the positional `label` so
    /// relabeling accounts never silently reorders priority.
    pub email: Option<String>,
    /// 5-hour utilization ratio in `[0.0, 1.0]`, if known.
    pub five_hour_ratio: Option<f32>,
    /// Weekly utilization ratio in `[0.0, 1.0]`, if known.
    pub seven_day_ratio: Option<f32>,
    /// The soonest reset instant this account reports (5h preferred, else 7d).
    pub resets_at: Option<String>,
    /// The weekly window's reset instant, if reported.
    pub seven_day_resets_at: Option<String>,
    /// Whether the account is fully exhausted (both windows >= ~99%).
    pub exhausted: bool,
    /// Whether the last fetch for this account errored (unusable this round).
    pub errored: bool,
    /// Whether this account's 5-hour window has ever been opened. An account at
    /// 0% with no 5h reset timestamp has never started its 5h clock and is a
    /// priming candidate.
    pub five_hour_window_open: bool,
}

impl AccountPace {
    /// The account's worst (highest) window utilization as a 0..=100 pct, used
    /// as a headroom proxy. `None` when neither window is known.
    pub fn binding_pct(&self) -> Option<f64> {
        match (self.five_hour_ratio, self.seven_day_ratio) {
            (Some(a), Some(b)) => Some(f64::from(a.max(b)) * 100.0),
            (Some(a), None) => Some(f64::from(a) * 100.0),
            (None, Some(b)) => Some(f64::from(b) * 100.0),
            (None, None) => None,
        }
    }

    /// The 5-hour window pace, if computable.
    pub fn five_hour_pace(&self, now: DateTime<Utc>) -> Option<WindowPace> {
        compute_window_pace(
            self.five_hour_ratio?,
            self.five_hour_resets_at(),
            FIVE_HOUR_PERIOD_SECS,
            now,
            0.0,
        )
    }

    /// The weekly window pace, if computable and outside the post-reset guard.
    pub fn seven_day_pace(&self, now: DateTime<Utc>) -> Option<WindowPace> {
        compute_window_pace(
            self.seven_day_ratio?,
            self.seven_day_resets_at.as_deref(),
            SEVEN_DAY_PERIOD_SECS,
            now,
            WEEKLY_SUPPRESS_AFTER_RESET_SECS,
        )
    }

    fn five_hour_resets_at(&self) -> Option<&str> {
        // Only the 5h window's own reset timestamp is valid for the 5h pace;
        // `resets_at` may carry the 7d fallback, so it is not reused here.
        self.resets_at
            .as_deref()
            .filter(|_| self.five_hour_window_open)
    }
}

/// Tuning for the balancing decision. Defaults mirror claude-swap's
/// `AutoSwitchSettings` (per-window thresholds, cooldown), plus a reset-time
/// hysteresis margin suited to a reset-ordered (consume-first) strategy.
#[derive(Debug, Clone, Copy)]
pub struct BalanceConfig {
    /// A candidate's 5-hour window must be strictly below this to be a valid
    /// landing (0..=100). Above it, the switch would re-fire immediately.
    pub five_hour_threshold_pct: f64,
    /// A candidate at/over this weekly utilization is deprioritized (not
    /// excluded) as a landing target (0..=100).
    pub seven_day_threshold_pct: f64,
    /// A candidate's weekly window must reset at least this many seconds sooner
    /// than the active account's to justify a proactive switch. This is the
    /// hysteresis for a reset-ordered strategy: two accounts whose weekly
    /// windows reset at nearly the same instant never ping-pong on measurement
    /// noise. (claude-swap's headroom-margin hysteresis belongs to its `best`
    /// strategy; consume-first is ordered by reset time, so the margin is a
    /// reset-time gap, not a headroom gap.)
    pub reset_hysteresis_secs: f64,
    /// Minimum seconds between proactive switches.
    pub cooldown_secs: f64,
    /// Priority strategy: at/over this binding utilization the active account is
    /// considered "capped" and the priority selector eagerly falls back to a
    /// lower-priority live account (0..=100).
    pub priority_capped_pct: f64,
    /// Priority strategy: asymmetric return hysteresis. A HIGHER-priority account
    /// is only "returnable" (worth switching back up to) once its binding
    /// utilization is this many percentage points below `priority_capped_pct`, so
    /// a primary hovering right at its cap after a reset does not cause the
    /// selector to flap back and forth (0..=100).
    pub priority_return_margin_pct: f64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        BalanceConfig {
            five_hour_threshold_pct: 85.0,
            seven_day_threshold_pct: 80.0,
            reset_hysteresis_secs: 30.0 * 60.0,
            cooldown_secs: 300.0,
            priority_capped_pct: 85.0,
            priority_return_margin_pct: 10.0,
        }
    }
}

/// Durable cooldown state so repeated ticks do not flip-flop between accounts.
#[derive(Debug, Clone, Copy, Default)]
pub struct BalanceState {
    /// When the last proactive switch happened (UTC epoch seconds), if any.
    pub last_switch_epoch: Option<f64>,
}

impl BalanceState {
    fn in_cooldown(&self, now: DateTime<Utc>, cooldown_secs: f64) -> bool {
        match self.last_switch_epoch {
            Some(last) => (now.timestamp() as f64 - last) < cooldown_secs,
            None => false,
        }
    }
}

/// The outcome of a balancing evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum BalanceDecision {
    /// Stay on the current account; the string is a short machine reason.
    Stay(&'static str),
    /// Switch to the named account for same-pace balancing.
    Switch { to: String, reason: &'static str },
    /// The current account is exhausted; switch to the best available peer.
    Failover { to: String },
    /// Every usable account is exhausted.
    AllExhausted,
    /// No usable peer this round (all errored / unknown / no candidates).
    Blocked(&'static str),
}

/// Choose a same-pace balancing target across the fleet.
///
/// Policy (mirrors claude-swap `consume-first`):
///
/// * If the current account is exhausted, this is a failover: pick the peer with
///   the most headroom, ignoring pace (any live account beats a dead one).
/// * Otherwise, keep the fleet balanced by proactively consuming the
///   soonest-resetting *weekly* window first (perishable, use-it-or-lose-it):
///   move to a peer whose weekly window resets strictly sooner than the active
///   account's, provided that peer's 5-hour window is below threshold. A peer
///   whose weekly window is at/over the weekly threshold is deprioritized, not
///   excluded, so 5-hour relief still wins when it is the only option.
/// * Hysteresis: a proactive switch only lands on a peer that beats the active
///   account's headroom by `hysteresis_pct`, so near-line pairs never ping-pong.
/// * Cooldown: no proactive switch while inside `cooldown_secs` of the last one.
///
/// `current_label` is the active account; `accounts` is the full fleet including
/// the active account.
pub fn select_balanced_target(
    current_label: &str,
    accounts: &[AccountPace],
    cfg: BalanceConfig,
    state: BalanceState,
    now: DateTime<Utc>,
) -> BalanceDecision {
    let current = accounts.iter().find(|a| a.label == current_label);
    let Some(current) = current else {
        return BalanceDecision::Blocked("current-account-unknown");
    };

    let usable: Vec<&AccountPace> = accounts
        .iter()
        .filter(|a| a.label != current_label && !a.errored)
        .collect();
    if usable.is_empty() {
        return BalanceDecision::Blocked("no-candidates");
    }

    // -- failover: current account is exhausted --------------------------------
    if current.exhausted {
        let best = usable.iter().filter(|a| !a.exhausted).min_by(|a, b| {
            let pa = a.binding_pct().unwrap_or(f64::INFINITY);
            let pb = b.binding_pct().unwrap_or(f64::INFINITY);
            pa.total_cmp(&pb)
        });
        return match best {
            Some(target) => BalanceDecision::Failover {
                to: target.label.clone(),
            },
            None => {
                if usable.iter().all(|a| a.exhausted) {
                    BalanceDecision::AllExhausted
                } else {
                    BalanceDecision::Blocked("no-headroom-candidate")
                }
            }
        };
    }

    // -- proactive same-pace balancing (consume-first) -------------------------
    if state.in_cooldown(now, cfg.cooldown_secs) {
        return BalanceDecision::Stay("cooldown");
    }

    let active_reset = future_seven_day_reset(current, now);

    // A candidate qualifies when its 5h window is below threshold and its weekly
    // window resets meaningfully sooner (by the reset-hysteresis margin) than
    // the active account's, so perishable weekly quota is consumed first.
    let mut qualifying: Vec<(SortKey, &AccountPace)> = Vec::new();
    for cand in &usable {
        if cand.exhausted {
            continue;
        }
        let cand_binding = match cand.binding_pct() {
            Some(p) => p,
            None => continue, // usage unknown this round - not a target
        };
        let five_h = cand
            .five_hour_ratio
            .map(|r| f64::from(r) * 100.0)
            .unwrap_or(0.0);
        if five_h >= cfg.five_hour_threshold_pct {
            continue; // would re-fire immediately
        }
        let cand_reset = future_seven_day_reset(cand, now);
        // Must reset sooner than active by the hysteresis margin. Two accounts
        // whose weekly windows reset at nearly the same instant never flip-flop.
        match (cand_reset, active_reset) {
            (Some(cr), Some(ar))
                if (ar.timestamp() - cr.timestamp()) as f64 >= cfg.reset_hysteresis_secs => {}
            _ => continue,
        }
        let seven_d = cand
            .seven_day_ratio
            .map(|r| f64::from(r) * 100.0)
            .unwrap_or(0.0);
        // 7d-heavy targets sink below lighter ones (deprioritize, not exclude).
        let heavy = if seven_d >= cfg.seven_day_threshold_pct {
            1
        } else {
            0
        };
        let reset_key = cand_reset.map(|r| r.timestamp()).unwrap_or(i64::MAX);
        qualifying.push((
            SortKey {
                heavy,
                reset_epoch: reset_key,
                neg_headroom: -(100.0 - cand_binding),
            },
            cand,
        ));
    }

    qualifying.sort_by(|a, b| a.0.cmp(&b.0));
    match qualifying.first() {
        Some((_, target)) => BalanceDecision::Switch {
            to: target.label.clone(),
            reason: "consume-first",
        },
        None => BalanceDecision::Stay("already-consuming-soonest"),
    }
}

/// Whether `account` matches priority-order `entry`, keyed on STABLE identity.
///
/// An entry matches the account's login email first (case-insensitive), and
/// falls back to an exact label match only when the account has no email or the
/// entry is not an email at all. This is why priority is robust to relabeling:
/// the positional `claude-1`/`claude-2` label is the last resort, not the key.
fn account_matches_priority_entry(account: &AccountPace, entry: &str) -> bool {
    let entry = entry.trim();
    if entry.is_empty() {
        return false;
    }
    if let Some(email) = account.email.as_deref()
        && email.eq_ignore_ascii_case(entry)
    {
        return true;
    }
    account.label == entry
}

/// Whether this account is a live landing target for priority selection: not
/// exhausted, not errored, and known-usable this round. Usage being unknown is
/// treated as usable so a fresh, never-probed higher-priority account is not
/// skipped forever - the priority list is an explicit operator preference.
fn priority_account_live(account: &AccountPace) -> bool {
    !account.exhausted && !account.errored
}

/// The account's binding (worst-window) utilization, or `0.0` when unknown, for
/// the priority strategy's capped/returnable thresholds. Unknown reads as
/// wide-open so a never-probed higher-priority account can still be returned to.
fn priority_binding_pct(account: &AccountPace) -> f64 {
    account.binding_pct().unwrap_or(0.0)
}

/// Choose a ranked-priority selection target across the fleet.
///
/// Policy (the captain's ranked-list strategy, distinct from consume-first pace
/// balancing):
///
/// * `priority_order` is a ranked list, most-preferred first, matched to
///   accounts by stable identity (email, then label) via
///   [`account_matches_priority_entry`]. Accounts not named in the list rank
///   after every named one, in fleet order, so an unlisted account is a
///   last-resort landing target rather than invisible.
/// * The selector prefers the HIGHEST-priority live account. If that account is
///   already current, it stays. If a higher-priority-than-current account is
///   live AND "returnable" (its binding utilization is comfortably below the cap
///   by `priority_return_margin_pct`), it returns up to it. This is how
///   "return on reset" happens for free: once the primary's window resets it is
///   live and comfortably below cap, so it wins the next evaluation.
/// * If the current account is exhausted, or capped (binding utilization at/over
///   `priority_capped_pct`), the selector falls back to the highest-priority
///   OTHER live account. An exhausted-current fallback is reported as
///   `Failover`; a merely-capped one as `Switch { reason: "priority-cap" }`.
/// * Asymmetric hysteresis: falling back off a capped primary is eager (fires at
///   the cap), while returning up to a higher-priority account is reluctant (only
///   once it is `priority_return_margin_pct` below the cap), so a primary
///   hovering at its cap never ping-pongs.
/// * Cooldown: no non-failover switch while inside `cooldown_secs` of the last
///   one. A failover off an exhausted current account ignores the cooldown, the
///   same way the exhaustion path always has.
///
/// `current_label` is the active account; `accounts` is the full fleet including
/// the active account.
pub fn select_priority_target(
    current_label: &str,
    accounts: &[AccountPace],
    priority_order: &[String],
    cfg: BalanceConfig,
    state: BalanceState,
    now: DateTime<Utc>,
) -> BalanceDecision {
    let Some(current) = accounts.iter().find(|a| a.label == current_label) else {
        return BalanceDecision::Blocked("current-account-unknown");
    };

    // Rank the fleet: accounts named in priority_order first (in list order),
    // then any unlisted account in fleet order. `rank_of` is the sort key.
    let rank_of = |account: &AccountPace| -> usize {
        priority_order
            .iter()
            .position(|entry| account_matches_priority_entry(account, entry))
            .unwrap_or(usize::MAX)
    };
    let current_rank = rank_of(current);

    // Live landing candidates other than the current account, best rank first.
    let mut ranked_live: Vec<&AccountPace> = accounts
        .iter()
        .filter(|a| a.label != current_label && priority_account_live(a))
        .collect();
    ranked_live.sort_by(|a, b| {
        rank_of(a)
            .cmp(&rank_of(b))
            .then_with(|| a.label.cmp(&b.label))
    });

    let current_exhausted = current.exhausted;
    let current_capped =
        current_exhausted || priority_binding_pct(current) >= cfg.priority_capped_pct;

    // -- current account is dead: eager failover, ignores cooldown --------------
    if current_exhausted {
        return match ranked_live.first() {
            Some(target) => BalanceDecision::Failover {
                to: target.label.clone(),
            },
            None => {
                if accounts.iter().filter(|a| !a.errored).all(|a| a.exhausted) {
                    BalanceDecision::AllExhausted
                } else {
                    BalanceDecision::Blocked("no-live-priority-candidate")
                }
            }
        };
    }

    if state.in_cooldown(now, cfg.cooldown_secs) {
        return BalanceDecision::Stay("cooldown");
    }

    // -- return up to a higher-priority, comfortably-below-cap account ----------
    // Reluctant: the higher account must be below the cap by the return margin so
    // a primary hovering at its cap after a reset does not flap.
    let return_ceiling = (cfg.priority_capped_pct - cfg.priority_return_margin_pct).max(0.0);
    if let Some(higher) = ranked_live
        .iter()
        .filter(|a| rank_of(a) < current_rank)
        .find(|a| priority_binding_pct(a) < return_ceiling)
    {
        return BalanceDecision::Switch {
            to: higher.label.clone(),
            reason: "priority-return",
        };
    }

    // -- current is capped (but not dead): eager fall back to the best live peer
    if current_capped {
        // Prefer a peer that is itself below the cap; otherwise take the best
        // ranked live peer anyway (any headroom beats a capped current account).
        let target = ranked_live
            .iter()
            .find(|a| priority_binding_pct(a) < cfg.priority_capped_pct)
            .or_else(|| ranked_live.first());
        return match target {
            Some(t) => BalanceDecision::Switch {
                to: t.label.clone(),
                reason: "priority-cap",
            },
            None => BalanceDecision::Stay("no-live-peer-while-capped"),
        };
    }

    BalanceDecision::Stay("priority-current-preferred")
}

fn future_seven_day_reset(account: &AccountPace, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let ts = parse_reset_ts(account.seven_day_resets_at.as_deref())?;
    // A stale snapshot can carry a resets_at that has already elapsed; treated
    // as a real instant it would rank the just-rolled-over account as
    // "soonest", so past == unknown (matches claude-swap `_seven_day_reset_ts`).
    if ts > now { Some(ts) } else { None }
}

#[derive(Debug, Clone, Copy)]
struct SortKey {
    heavy: u8,
    reset_epoch: i64,
    neg_headroom: f64,
}

impl PartialEq for SortKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for SortKey {}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.heavy
            .cmp(&other.heavy)
            .then(self.reset_epoch.cmp(&other.reset_epoch))
            .then(self.neg_headroom.total_cmp(&other.neg_headroom))
    }
}

/// Whether priming an unopened 5-hour window is worth it right now.
///
/// Priming opens an account's 5-hour clock with a minimal request so the window
/// is already counting down when the fleet needs it. It is only worth the
/// (small) quota cost when more capacity will be needed soon, defined as:
///
/// * `candidate` has never opened its 5-hour window (`five_hour_window_open ==
///   false`), is not exhausted, and did not error, AND
/// * the active account is under real pressure - either already ahead of pace on
///   some window, or its binding utilization is within `pressure_margin_pct` of
///   the 5-hour threshold - so its 5-hour window is likely to bind soon and the
///   fleet will want a warmed-up peer to fail over onto.
///
/// Returns `false` for an account whose window is already open (nothing to
/// prime) and whenever the fleet has plenty of slack (priming would just burn a
/// little quota for no benefit).
pub fn should_prime(
    candidate: &AccountPace,
    active: &AccountPace,
    cfg: BalanceConfig,
    now: DateTime<Utc>,
    pressure_margin_pct: f64,
) -> bool {
    if candidate.five_hour_window_open || candidate.exhausted || candidate.errored {
        return false;
    }
    // Active account under pressure?
    let ahead = active
        .five_hour_pace(now)
        .map(|p| p.ahead_of_pace(AHEAD_THRESHOLD_PCT))
        .unwrap_or(false)
        || active
            .seven_day_pace(now)
            .map(|p| p.ahead_of_pace(AHEAD_THRESHOLD_PCT))
            .unwrap_or(false);
    let near_threshold = active
        .five_hour_ratio
        .map(|r| f64::from(r) * 100.0 >= cfg.five_hour_threshold_pct - pressure_margin_pct)
        .unwrap_or(false);
    ahead || near_threshold
}

#[cfg(test)]
#[path = "pace_tests.rs"]
mod pace_tests;

use super::*;
use chrono::{Duration, TimeZone};

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

fn account(label: &str) -> AccountPace {
    AccountPace {
        label: label.to_string(),
        five_hour_ratio: Some(0.0),
        seven_day_ratio: Some(0.0),
        resets_at: None,
        seven_day_resets_at: None,
        exhausted: false,
        errored: false,
        five_hour_window_open: true,
    }
}

// ── pace math ────────────────────────────────────────────────────────────────

#[test]
fn weekly_pace_ahead_when_usage_outruns_elapsed_fraction() {
    // now is ~2 days into a 7-day cycle => expected ~28.6%. 60% used is ahead.
    let now = utc(2026, 8, 6, 0, 0);
    let reset = now + Duration::days(5); // 2 days elapsed of 7
    let pace = compute_window_pace(
        0.60,
        Some(&reset.to_rfc3339()),
        SEVEN_DAY_PERIOD_SECS,
        now,
        WEEKLY_SUPPRESS_AFTER_RESET_SECS,
    )
    .expect("pace computable");
    assert!(
        (pace.expected_pct - 2.0 / 7.0 * 100.0).abs() < 0.5,
        "expected {}",
        pace.expected_pct
    );
    assert!(
        (pace.actual_pct - 60.0).abs() < 0.01,
        "actual {}",
        pace.actual_pct
    );
    assert!(pace.ahead_of_pace(AHEAD_THRESHOLD_PCT));
    let ratio = pace.pace_ratio.unwrap();
    assert!(ratio > 2.0, "ratio {}", ratio);
}

#[test]
fn weekly_pace_behind_when_usage_below_elapsed_fraction() {
    let now = utc(2026, 8, 6, 0, 0);
    let reset = now + Duration::days(2); // 5 days elapsed of 7 => expected ~71%
    let pace = compute_window_pace(
        0.40,
        Some(&reset.to_rfc3339()),
        SEVEN_DAY_PERIOD_SECS,
        now,
        WEEKLY_SUPPRESS_AFTER_RESET_SECS,
    )
    .unwrap();
    assert!(!pace.ahead_of_pace(AHEAD_THRESHOLD_PCT));
    assert!(pace.pace_ratio.unwrap() < 1.0);
    assert_eq!(pace.will_last_to_reset(), Some(true));
}

#[test]
fn weekly_pace_suppressed_right_after_reset() {
    let now = utc(2026, 8, 6, 0, 0);
    let reset = now + Duration::days(7) - Duration::hours(1); // 1h elapsed
    let pace = compute_window_pace(
        0.30,
        Some(&reset.to_rfc3339()),
        SEVEN_DAY_PERIOD_SECS,
        now,
        WEEKLY_SUPPRESS_AFTER_RESET_SECS,
    );
    assert!(pace.is_none(), "should suppress within 24h of reset");
}

#[test]
fn pace_none_without_reset_timestamp() {
    let now = utc(2026, 8, 6, 0, 0);
    assert!(compute_window_pace(0.5, None, SEVEN_DAY_PERIOD_SECS, now, 0.0).is_none());
}

#[test]
fn five_hour_pace_not_suppressed_early() {
    // 5h window uses suppress=0, so it computes even 30m in.
    let now = utc(2026, 8, 6, 0, 0);
    let reset = now + Duration::minutes(270); // 30m elapsed of 300m
    let pace = compute_window_pace(
        0.50,
        Some(&reset.to_rfc3339()),
        FIVE_HOUR_PERIOD_SECS,
        now,
        0.0,
    )
    .unwrap();
    assert!((pace.expected_pct - 10.0).abs() < 0.5);
    assert!(pace.ahead_of_pace(AHEAD_THRESHOLD_PCT));
}

#[test]
fn will_last_to_reset_false_when_burning_too_fast() {
    let now = utc(2026, 8, 6, 0, 0);
    let reset = now + Duration::days(6); // 1 day elapsed, but suppress is 24h exactly
    // push elapsed just over 24h to escape suppression
    let reset = reset - Duration::hours(1);
    let pace = compute_window_pace(
        0.50,
        Some(&reset.to_rfc3339()),
        SEVEN_DAY_PERIOD_SECS,
        now,
        WEEKLY_SUPPRESS_AFTER_RESET_SECS,
    )
    .unwrap();
    // 50% in ~1.04 days => projects way over 100% by day 7.
    assert_eq!(pace.will_last_to_reset(), Some(false));
}

// ── balancing decision ───────────────────────────────────────────────────────

#[test]
fn balancing_consumes_soonest_resetting_weekly_first() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    // active resets late (7 days out), moderate weekly load.
    current.seven_day_ratio = Some(0.30);
    current.seven_day_resets_at = Some((now + Duration::days(6)).to_rfc3339());

    let mut peer = account("b");
    // peer resets SOON (1 day) with low 5h and lower weekly => consume it first.
    peer.five_hour_ratio = Some(0.10);
    peer.seven_day_ratio = Some(0.20);
    peer.seven_day_resets_at = Some((now + Duration::days(1)).to_rfc3339());

    let decision = select_balanced_target(
        "a",
        &[current, peer],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(
        decision,
        BalanceDecision::Switch {
            to: "b".to_string(),
            reason: "consume-first"
        }
    );
}

#[test]
fn balancing_stays_when_already_on_soonest() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.seven_day_ratio = Some(0.30);
    current.seven_day_resets_at = Some((now + Duration::days(1)).to_rfc3339()); // soonest

    let mut peer = account("b");
    peer.seven_day_ratio = Some(0.20);
    peer.seven_day_resets_at = Some((now + Duration::days(6)).to_rfc3339());

    let decision = select_balanced_target(
        "a",
        &[current, peer],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(decision, BalanceDecision::Stay("already-consuming-soonest"));
}

#[test]
fn balancing_respects_cooldown_no_flip_flop() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.seven_day_ratio = Some(0.30);
    current.seven_day_resets_at = Some((now + Duration::days(6)).to_rfc3339());
    let mut peer = account("b");
    peer.five_hour_ratio = Some(0.10);
    peer.seven_day_ratio = Some(0.20);
    peer.seven_day_resets_at = Some((now + Duration::days(1)).to_rfc3339());

    // last switch 60s ago, cooldown 300s => must stay.
    let state = BalanceState {
        last_switch_epoch: Some(now.timestamp() as f64 - 60.0),
    };
    let decision =
        select_balanced_target("a", &[current, peer], BalanceConfig::default(), state, now);
    assert_eq!(decision, BalanceDecision::Stay("cooldown"));
}

#[test]
fn balancing_reset_hysteresis_blocks_near_simultaneous_reset() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.five_hour_ratio = Some(0.50);
    current.seven_day_ratio = Some(0.30);
    current.seven_day_resets_at = Some((now + Duration::days(2)).to_rfc3339());

    // peer resets only 10 minutes sooner than active - inside the 30-minute
    // reset-hysteresis margin, so no proactive switch (no ping-pong on noise).
    let mut peer = account("b");
    peer.five_hour_ratio = Some(0.10);
    peer.seven_day_ratio = Some(0.20);
    peer.seven_day_resets_at = Some((now + Duration::days(2) - Duration::minutes(10)).to_rfc3339());

    let decision = select_balanced_target(
        "a",
        &[current, peer],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(decision, BalanceDecision::Stay("already-consuming-soonest"));
}

#[test]
fn balancing_failover_picks_most_headroom_when_current_exhausted() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.exhausted = true;
    current.five_hour_ratio = Some(1.0);
    current.seven_day_ratio = Some(1.0);

    let mut low = account("b");
    low.five_hour_ratio = Some(0.70);
    low.seven_day_ratio = Some(0.70);
    let mut high = account("c");
    high.five_hour_ratio = Some(0.10);
    high.seven_day_ratio = Some(0.10);

    let decision = select_balanced_target(
        "a",
        &[current, low, high],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(
        decision,
        BalanceDecision::Failover {
            to: "c".to_string()
        }
    );
}

#[test]
fn balancing_all_exhausted() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.exhausted = true;
    let mut peer = account("b");
    peer.exhausted = true;
    let decision = select_balanced_target(
        "a",
        &[current, peer],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(decision, BalanceDecision::AllExhausted);
}

#[test]
fn balancing_deprioritizes_7d_heavy_but_still_uses_for_5h_relief() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut current = account("a");
    current.seven_day_ratio = Some(0.30);
    current.seven_day_resets_at = Some((now + Duration::days(6)).to_rfc3339());

    // Only candidate resets sooner, low 5h but heavy 7d (>= 80%): still taken.
    let mut heavy = account("b");
    heavy.five_hour_ratio = Some(0.05);
    heavy.seven_day_ratio = Some(0.85);
    heavy.seven_day_resets_at = Some((now + Duration::days(1)).to_rfc3339());

    let decision = select_balanced_target(
        "a",
        &[current, heavy],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(
        decision,
        BalanceDecision::Switch {
            to: "b".to_string(),
            reason: "consume-first"
        }
    );
}

#[test]
fn balancing_blocked_when_no_candidates() {
    let now = utc(2026, 8, 6, 0, 0);
    let current = account("a");
    let decision = select_balanced_target(
        "a",
        &[current],
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(decision, BalanceDecision::Blocked("no-candidates"));
}

// ── priming ──────────────────────────────────────────────────────────────────

#[test]
fn prime_when_active_under_pressure_and_candidate_unopened() {
    let now = utc(2026, 8, 6, 0, 0);
    // candidate never opened 5h window (the claude-3 case: 0%/0%, no resets).
    let mut cand = account("c");
    cand.five_hour_ratio = Some(0.0);
    cand.seven_day_ratio = Some(0.0);
    cand.five_hour_window_open = false;
    cand.resets_at = None;
    cand.seven_day_resets_at = None;

    // active near the 5h threshold (85% - margin 10% => 75%).
    let mut active = account("a");
    active.five_hour_ratio = Some(0.80);

    assert!(should_prime(
        &cand,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

#[test]
fn no_prime_when_fleet_has_slack() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut cand = account("c");
    cand.five_hour_window_open = false;
    let mut active = account("a");
    active.five_hour_ratio = Some(0.10); // plenty of slack, not ahead of pace
    assert!(!should_prime(
        &cand,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

#[test]
fn no_prime_when_window_already_open() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut cand = account("c");
    cand.five_hour_window_open = true; // already primed
    let mut active = account("a");
    active.five_hour_ratio = Some(0.90);
    assert!(!should_prime(
        &cand,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

#[test]
fn no_prime_exhausted_or_errored_candidate() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut active = account("a");
    active.five_hour_ratio = Some(0.90);

    let mut exhausted = account("c");
    exhausted.five_hour_window_open = false;
    exhausted.exhausted = true;
    assert!(!should_prime(
        &exhausted,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));

    let mut errored = account("d");
    errored.five_hour_window_open = false;
    errored.errored = true;
    assert!(!should_prime(
        &errored,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

#[test]
fn prime_when_active_ahead_of_pace_even_below_threshold() {
    let now = utc(2026, 8, 6, 0, 0);
    let mut cand = account("c");
    cand.five_hour_window_open = false;

    // active only 40% used but far ahead of weekly pace (40% used, ~10% elapsed).
    let mut active = account("a");
    active.five_hour_ratio = Some(0.40);
    active.seven_day_ratio = Some(0.40);
    let reset = now + Duration::days(7) - Duration::hours(25); // ~25h elapsed
    active.seven_day_resets_at = Some(reset.to_rfc3339());

    assert!(should_prime(
        &cand,
        &active,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

// ── real `jcode usage` fleet scenario (verified against live output) ──────────
//
// Reproduces the exact fleet the task was written against, captured from real
// `jcode usage --json` at build time:
//   claude-2 (active): 5h 13%, 7d 65%, 7d resets ~4d7h out
//   claude-1:          5h  0%, 7d 86%, 7d resets ~1d14h out (5h window never opened)
//   claude-3:          5h  0%, 7d  0%, no resets (never opened either window)
//   claude-4:          token expired (errored)

fn real_fleet(now: DateTime<Utc>) -> Vec<AccountPace> {
    vec![
        AccountPace {
            label: "claude-2".to_string(),
            five_hour_ratio: Some(0.13),
            seven_day_ratio: Some(0.65),
            resets_at: Some((now + Duration::hours(2) + Duration::minutes(47)).to_rfc3339()),
            seven_day_resets_at: Some((now + Duration::days(4) + Duration::hours(7)).to_rfc3339()),
            exhausted: false,
            errored: false,
            five_hour_window_open: true,
        },
        AccountPace {
            label: "claude-1".to_string(),
            five_hour_ratio: Some(0.0),
            seven_day_ratio: Some(0.86),
            resets_at: None,
            seven_day_resets_at: Some((now + Duration::days(1) + Duration::hours(14)).to_rfc3339()),
            exhausted: false,
            errored: false,
            five_hour_window_open: false,
        },
        AccountPace {
            label: "claude-3".to_string(),
            five_hour_ratio: Some(0.0),
            seven_day_ratio: Some(0.0),
            resets_at: None,
            seven_day_resets_at: None,
            exhausted: false,
            errored: false,
            five_hour_window_open: false,
        },
        AccountPace {
            label: "claude-4".to_string(),
            five_hour_ratio: None,
            seven_day_ratio: None,
            resets_at: None,
            seven_day_resets_at: None,
            exhausted: false,
            errored: true,
            five_hour_window_open: false,
        },
    ]
}

#[test]
fn real_fleet_consumes_soonest_resetting_weekly_first() {
    let now = utc(2026, 8, 6, 5, 20);
    let fleet = real_fleet(now);
    // active claude-2 resets in ~4d; claude-1 resets in ~1.6d with 5h idle, so
    // the soonest-resetting perishable weekly quota is claude-1's - consume it.
    let decision = select_balanced_target(
        "claude-2",
        &fleet,
        BalanceConfig::default(),
        BalanceState::default(),
        now,
    );
    assert_eq!(
        decision,
        BalanceDecision::Switch {
            to: "claude-1".to_string(),
            reason: "consume-first"
        }
    );
}

#[test]
fn real_fleet_primes_never_opened_account_under_pressure() {
    let now = utc(2026, 8, 6, 5, 20);
    let fleet = real_fleet(now);
    let probe_active = &fleet[0]; // claude-2
    let cand = &fleet[2]; // claude-3, never opened 5h window
    // claude-2 is 65% into its weekly window with only ~2.7 days elapsed of 7
    // (expected ~39%), so it is meaningfully ahead of weekly pace - priming a
    // warm peer now is worthwhile.
    assert!(should_prime(
        cand,
        probe_active,
        BalanceConfig::default(),
        now,
        10.0
    ));

    // A relaxed active account (well behind pace, low 5h) needs no priming.
    let mut relaxed = probe_active.clone();
    relaxed.five_hour_ratio = Some(0.05);
    relaxed.seven_day_ratio = Some(0.10);
    assert!(!should_prime(
        cand,
        &relaxed,
        BalanceConfig::default(),
        now,
        10.0
    ));
}

#[test]
fn real_fleet_prime_candidate_selects_never_opened_account() {
    let now = utc(2026, 8, 6, 5, 20);
    let mut fleet = real_fleet(now);
    fleet[0].five_hour_ratio = Some(0.80); // active under 5h pressure
    let active = &fleet[0];
    let target = fleet
        .iter()
        .filter(|c| c.label != active.label)
        .find(|c| should_prime(c, active, BalanceConfig::default(), now, 10.0))
        .map(|c| c.label.clone());
    // claude-1 and claude-3 both have unopened 5h windows and both qualify to
    // prime; the first unopened peer wins.
    let target = target.expect("a prime candidate under pressure");
    assert!(target == "claude-1" || target == "claude-3", "got {target}");
}

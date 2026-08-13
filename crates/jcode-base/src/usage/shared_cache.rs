//! Host-wide shared usage cache (L2), converged with `quota-axi`.
//!
//! Each live jcode session keeps its own in-memory usage cache (L1, see
//! [`super::cache`]). With N live sessions that is N independent pollers hitting
//! the provider usage endpoints, which is the likely cause of the constant 429s
//! we observe. `quota-axi` owns one shared on-disk cache that every local agent
//! tool can read and write, so the whole host makes roughly one usage fetch per
//! provider-account per TTL regardless of session count.
//!
//! This module reads and writes THAT SAME FILE so jcode and `quota-axi`
//! converge on one cache rather than maintaining parallel formats:
//!
//! - Location: `${XDG_CACHE_HOME:-~/.cache}/quota-axi/quotas.json` (identical to
//!   `quota-axi`'s `cacheFilePath()`; deliberately NOT sandboxed under
//!   `JCODE_HOME`, because the whole point is a cross-tool shared file).
//! - Format: `{ "generatedAt", "schemaVersion": 1, "providers": [...] }` with
//!   one record per provider id, ordered by `quota-axi`'s `PROVIDER_IDS`.
//! - Writes are atomic (temp file + rename) with `0o600` permissions, matching
//!   `quota-axi`'s `writeCacheFile`.
//!
//! ## Scope: active account only
//!
//! `quota-axi` reads whatever the current OAuth credentials resolve to and
//! stores exactly one record per provider; the persisted record carries no
//! account identity. jcode is multi-account, but its active account is
//! host-global state on disk (`auth::claude::active_account_label` /
//! `auth::codex::active_account_label`), read the same way by every session and
//! by `quota-axi`. So the shared file consistently describes the host's active
//! account. We therefore read/write L2 only for the active account; non-active
//! account probes (used during switching) keep using L1 + direct fetch. That is
//! also exactly the account the info widget and `/usage` poll constantly, which
//! is the N-poller problem this ticket targets.
//!
//! ## Backoff semantics are preserved
//!
//! `quota-axi` only ever persists successful (`fresh`) records with windows;
//! errors and 429s are never written to the shared file. jcode does the same:
//! only successful fetches are written through to L2, and all error/rate-limit
//! backoff (the existing 900s-on-429 behavior) stays entirely in L1. The shared
//! file can therefore never mask an error or shorten a backoff.

use super::model::ModelScopedUsageWindow;
use super::{OpenAIUsageData, OpenAIUsageWindow, UsageData};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

/// Freshness horizon for a shared-cache record, matching jcode's L1
/// `CACHE_DURATION` and `quota-axi`'s ~5min TTL. A record older than this (by
/// its `state.refreshedAt`) is ignored on read so a stale shared file cannot
/// pin every session to old data.
const SHARED_CACHE_TTL_SECS: i64 = 300;

const FIVE_HOURS_SECONDS: u64 = 18_000;
const SEVEN_DAYS_SECONDS: u64 = 604_800;

/// Provider ordering used by `quota-axi` when it rewrites the file. We match it
/// so our writes keep the same on-disk layout.
const PROVIDER_IDS: [&str; 6] = ["claude", "codex", "cursor", "copilot", "grok", "kimi"];

// ─── On-disk schema (mirrors quota-axi's persisted subset) ───────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    #[serde(rename = "generatedAt")]
    generated_at: String,
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    providers: Vec<CacheProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheProvider {
    provider: String,
    label: String,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    windows: Vec<CacheWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credits: Option<serde_json::Value>,
    state: CacheState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheWindow {
    id: String,
    label: String,
    kind: String,
    #[serde(rename = "percentUsed", skip_serializing_if = "Option::is_none")]
    percent_used: Option<f64>,
    #[serde(rename = "percentRemaining", skip_serializing_if = "Option::is_none")]
    percent_remaining: Option<f64>,
    #[serde(rename = "startsAt", skip_serializing_if = "Option::is_none")]
    starts_at: Option<String>,
    #[serde(rename = "resetsAt", skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
    #[serde(rename = "resetText", skip_serializing_if = "Option::is_none")]
    reset_text: Option<String>,
    #[serde(rename = "windowSeconds", skip_serializing_if = "Option::is_none")]
    window_seconds: Option<u64>,
    #[serde(rename = "spentUsd", skip_serializing_if = "Option::is_none")]
    spent_usd: Option<f64>,
    #[serde(rename = "limitUsd", skip_serializing_if = "Option::is_none")]
    limit_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheState {
    status: String,
    stale: bool,
    #[serde(rename = "refreshedAt", skip_serializing_if = "Option::is_none")]
    refreshed_at: Option<String>,
    #[serde(rename = "sourcesTried")]
    sources_tried: Vec<String>,
}

// ─── File location and IO ────────────────────────────────────────────────────

/// The shared cache file path, byte-identical to `quota-axi`'s `cacheFilePath()`.
fn cache_file_path() -> Option<PathBuf> {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()?.join(".cache"),
    };
    Some(base.join("quota-axi").join("quotas.json"))
}

fn read_cache_file() -> Option<CacheFile> {
    let path = cache_file_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let file: CacheFile = serde_json::from_str(&text).ok()?;
    if file.schema_version != 1 {
        return None;
    }
    Some(file)
}

/// Atomically write the merged cache file with owner-only permissions, matching
/// `quota-axi`'s `writeCacheFile` (temp file + rename, `0o600`, 2-space indent,
/// trailing newline).
fn write_cache_file(providers: Vec<CacheProvider>) {
    let Some(path) = cache_file_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = jcode_core::fs::set_directory_permissions_owner_only(parent);

    let file = CacheFile {
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        schema_version: 1,
        providers,
    };
    let Ok(mut body) = serde_json::to_string_pretty(&file) else {
        return;
    };
    body.push('\n');

    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&temp, body.as_bytes()).is_err() {
        return;
    }
    let _ = jcode_core::fs::set_permissions_owner_only(&temp);
    if std::fs::rename(&temp, &path).is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    let _ = jcode_core::fs::set_permissions_owner_only(&path);
}

/// Merge one provider record into the existing file (read-modify-write),
/// preserving other providers and re-emitting in `PROVIDER_IDS` order. Mirrors
/// `quota-axi`'s `writeCachedProviders` merge.
fn upsert_provider(record: CacheProvider) {
    let mut by_provider: std::collections::HashMap<String, CacheProvider> = read_cache_file()
        .map(|file| {
            file.providers
                .into_iter()
                .map(|p| (p.provider.clone(), p))
                .collect()
        })
        .unwrap_or_default();

    by_provider.insert(record.provider.clone(), record);

    let merged = PROVIDER_IDS
        .iter()
        .filter_map(|id| by_provider.remove(*id))
        .collect::<Vec<_>>();
    write_cache_file(merged);
}

fn provider_record(id: &str) -> Option<CacheProvider> {
    read_cache_file()?
        .providers
        .into_iter()
        .find(|p| p.provider == id)
}

/// Whether a `fresh` record is still within the shared TTL, judged by its
/// `state.refreshedAt`. Records without a parseable timestamp are treated as
/// stale so a malformed file never pins sessions to old data.
fn record_is_fresh(record: &CacheProvider) -> bool {
    if record.state.status != "fresh" {
        return false;
    }
    let Some(refreshed_at) = record.state.refreshed_at.as_deref() else {
        return false;
    };
    let Ok(refreshed) = chrono::DateTime::parse_from_rfc3339(refreshed_at) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(refreshed.with_timezone(&chrono::Utc));
    age.num_seconds() >= 0 && age.num_seconds() < SHARED_CACHE_TTL_SECS
}

// ─── Active-account gating ───────────────────────────────────────────────────

/// The active account is host-global disk state, so a fetch for `label` shares
/// the L2 file exactly when `label` is the active account. A `None` active label
/// means the single-account/default case, which is always active.
pub(super) fn anthropic_account_is_active(label: &str) -> bool {
    match crate::auth::claude::active_account_label() {
        Some(active) => active == label,
        None => true,
    }
}

pub(super) fn openai_account_is_active(label: Option<&str>) -> bool {
    match (crate::auth::codex::active_account_label(), label) {
        (Some(active), Some(label)) => active == label,
        _ => true,
    }
}

// ─── Anthropic (claude) conversions ──────────────────────────────────────────

fn f32_percent(ratio: f32) -> f64 {
    ((ratio * 100.0).clamp(0.0, 100.0)) as f64
}

fn percent_remaining(percent_used: f64) -> f64 {
    (100.0 - percent_used).clamp(0.0, 100.0)
}

fn claude_window(
    id: &str,
    label: &str,
    kind: &str,
    ratio: f32,
    resets_at: Option<&str>,
    window_seconds: u64,
) -> CacheWindow {
    let percent_used = f32_percent(ratio);
    CacheWindow {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.to_string(),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining(percent_used)),
        starts_at: None,
        resets_at: resets_at.map(str::to_string),
        reset_text: None,
        window_seconds: Some(window_seconds),
        spent_usd: None,
        limit_usd: None,
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_us = true; // trim leading underscores
    for ch in value.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn usage_data_to_windows(data: &UsageData) -> Vec<CacheWindow> {
    let mut windows = vec![
        claude_window(
            "five_hour",
            "session",
            "session",
            data.five_hour,
            data.five_hour_resets_at.as_deref(),
            FIVE_HOURS_SECONDS,
        ),
        claude_window(
            "seven_day",
            "week",
            "weekly",
            data.seven_day,
            data.seven_day_resets_at.as_deref(),
            SEVEN_DAYS_SECONDS,
        ),
    ];
    if let Some(opus) = data.seven_day_opus {
        windows.push(claude_window(
            "seven_day_opus",
            "opus week",
            "model",
            opus,
            data.seven_day_resets_at.as_deref(),
            SEVEN_DAYS_SECONDS,
        ));
    }
    for scoped in &data.model_scoped {
        windows.push(claude_window(
            &format!("model:{}", slugify(&scoped.model_name)),
            &format!("{} week", scoped.model_name),
            "model",
            scoped.utilization,
            scoped.resets_at.as_deref(),
            SEVEN_DAYS_SECONDS,
        ));
    }
    windows
}

fn windows_to_usage_data(record: &CacheProvider) -> UsageData {
    let ratio_of = |w: &CacheWindow| -> f32 {
        (w.percent_used.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32
    };
    let find = |id: &str| record.windows.iter().find(|w| w.id == id);

    let five = find("five_hour");
    let seven = find("seven_day");
    let opus = find("seven_day_opus");
    let model_scoped = record
        .windows
        .iter()
        .filter(|w| w.kind == "model" && w.id.starts_with("model:"))
        .map(|w| ModelScopedUsageWindow {
            model_name: w
                .label
                .strip_suffix(" week")
                .unwrap_or(&w.label)
                .to_string(),
            utilization: ratio_of(w),
            resets_at: w.resets_at.clone(),
        })
        .collect();

    UsageData {
        five_hour: five.map(ratio_of).unwrap_or(0.0),
        five_hour_resets_at: five.and_then(|w| w.resets_at.clone()),
        seven_day: seven.map(ratio_of).unwrap_or(0.0),
        seven_day_resets_at: seven.and_then(|w| w.resets_at.clone()),
        seven_day_opus: opus.map(ratio_of),
        model_scoped,
        extra_usage_enabled: record.windows.iter().any(|w| w.id == "extra_usage"),
        fetched_at: Some(Instant::now()),
        last_error: None,
    }
}

/// Read the active Anthropic account's usage from the shared file, if a fresh
/// record is present. `None` on any miss (no file, stale, wrong provider).
pub(super) fn read_anthropic() -> Option<UsageData> {
    let record = provider_record("claude")?;
    if record.source == "unavailable" || !record_is_fresh(&record) || record.windows.is_empty() {
        return None;
    }
    Some(windows_to_usage_data(&record))
}

/// Write the active Anthropic account's successful usage to the shared file.
/// No-op for error states so backoff stays owned by L1.
pub(super) fn write_anthropic(data: &UsageData) {
    if data.last_error.is_some() {
        return;
    }
    let windows = usage_data_to_windows(data);
    if windows.is_empty() {
        return;
    }
    upsert_provider(CacheProvider {
        provider: "claude".to_string(),
        label: "Claude".to_string(),
        source: "oauth".to_string(),
        plan: None,
        windows,
        credits: None,
        state: CacheState {
            status: "fresh".to_string(),
            stale: false,
            refreshed_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            sources_tried: vec!["oauth".to_string()],
        },
    });
}

// ─── OpenAI (codex) conversions ──────────────────────────────────────────────
//
// `quota-axi` validates codex window identities strictly: an unrecognized id
// makes it reject the whole codex record on read. We therefore write only the
// two primary windows with their exact identities (`five_hour`/session and
// `seven_day`/weekly), which are the windows the info widget and `/usage` poll
// constantly. jcode's `spark` window has no `quota-axi` codex identity we can
// reconstruct without the upstream limit id, so it stays L1-only.

fn openai_window(id: &str, label: &str, kind: &str, window: &OpenAIUsageWindow, secs: u64) -> CacheWindow {
    let percent_used = f32_percent(window.usage_ratio);
    CacheWindow {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.to_string(),
        percent_used: Some(percent_used),
        percent_remaining: Some(percent_remaining(percent_used)),
        starts_at: None,
        resets_at: window.resets_at.clone(),
        reset_text: None,
        window_seconds: Some(secs),
        spent_usd: None,
        limit_usd: None,
    }
}

fn openai_data_to_windows(data: &OpenAIUsageData) -> Vec<CacheWindow> {
    let mut windows = Vec::new();
    if let Some(window) = &data.five_hour {
        windows.push(openai_window(
            "five_hour",
            "session",
            "session",
            window,
            FIVE_HOURS_SECONDS,
        ));
    }
    if let Some(window) = &data.seven_day {
        windows.push(openai_window(
            "seven_day",
            "week",
            "weekly",
            window,
            SEVEN_DAYS_SECONDS,
        ));
    }
    windows
}

fn windows_to_openai_data(record: &CacheProvider) -> OpenAIUsageData {
    let to_window = |w: &CacheWindow, name: &str| OpenAIUsageWindow {
        name: name.to_string(),
        usage_ratio: (w.percent_used.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32,
        resets_at: w.resets_at.clone(),
    };
    let five = record
        .windows
        .iter()
        .find(|w| w.id == "five_hour")
        .map(|w| to_window(w, "5-hour window"));
    let seven = record
        .windows
        .iter()
        .find(|w| w.id == "seven_day")
        .map(|w| to_window(w, "7-day window"));

    OpenAIUsageData {
        five_hour: five,
        seven_day: seven,
        spark: None,
        hard_limit_reached: false,
        fetched_at: Some(Instant::now()),
        last_error: None,
    }
}

/// Read the active OpenAI account's usage from the shared file, if fresh.
pub(super) fn read_openai() -> Option<OpenAIUsageData> {
    let record = provider_record("codex")?;
    if record.source == "unavailable" || !record_is_fresh(&record) || record.windows.is_empty() {
        return None;
    }
    let data = windows_to_openai_data(&record);
    data.has_limits().then_some(data)
}

/// Write the active OpenAI account's successful usage to the shared file.
/// No-op for errors so backoff stays owned by L1.
pub(super) fn write_openai(data: &OpenAIUsageData) {
    if data.last_error.is_some() {
        return;
    }
    let windows = openai_data_to_windows(data);
    if windows.is_empty() {
        return;
    }
    upsert_provider(CacheProvider {
        provider: "codex".to_string(),
        label: "Codex".to_string(),
        source: "oauth".to_string(),
        plan: None,
        windows,
        credits: None,
        state: CacheState {
            status: "fresh".to_string(),
            stale: false,
            refreshed_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            sources_tried: vec!["oauth".to_string()],
        },
    });
}

#[cfg(test)]
mod tests;

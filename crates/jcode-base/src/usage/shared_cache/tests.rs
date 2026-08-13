use super::*;
use crate::usage::{OpenAIUsageData, OpenAIUsageWindow, UsageData};
use std::sync::Mutex;

// XDG_CACHE_HOME is process-global, so serialize the tests that redirect it to a
// temp directory. Each test uses its own temp dir and restores the prior value.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct CacheEnv {
    _dir: tempfile::TempDir,
    prev: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl CacheEnv {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        // SAFETY: single-threaded test scope, serialized by ENV_LOCK.
        unsafe { std::env::set_var("XDG_CACHE_HOME", dir.path()) };
        Self {
            _dir: dir,
            prev,
            _guard: guard,
        }
    }
}

impl Drop for CacheEnv {
    fn drop(&mut self) {
        // SAFETY: single-threaded test scope, serialized by ENV_LOCK.
        unsafe {
            match &self.prev {
                Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
    }
}

fn sample_usage() -> UsageData {
    UsageData {
        five_hour: 0.42,
        five_hour_resets_at: Some("2999-01-01T00:00:00Z".to_string()),
        seven_day: 0.15,
        seven_day_resets_at: Some("2999-01-08T00:00:00Z".to_string()),
        seven_day_opus: Some(0.3),
        model_scoped: vec![ModelScopedUsageWindow {
            model_name: "Fable".to_string(),
            utilization: 0.2,
            resets_at: Some("2999-01-08T00:00:00Z".to_string()),
        }],
        extra_usage_enabled: false,
        fetched_at: Some(Instant::now()),
        last_error: None,
    }
}

#[test]
fn anthropic_write_then_read_round_trips_active_account() {
    let _env = CacheEnv::new();
    let data = sample_usage();
    write_anthropic(&data);

    let read = read_anthropic().expect("fresh record present");
    assert!((read.five_hour - 0.42).abs() < 0.01);
    assert!((read.seven_day - 0.15).abs() < 0.01);
    assert_eq!(read.seven_day_opus.map(|v| (v * 100.0).round()), Some(30.0));
    assert_eq!(read.model_scoped.len(), 1);
    assert_eq!(read.model_scoped[0].model_name, "Fable");
    assert!((read.model_scoped[0].utilization - 0.2).abs() < 0.01);
}

#[test]
fn written_file_matches_quota_axi_schema() {
    let _env = CacheEnv::new();
    write_anthropic(&sample_usage());

    let path = cache_file_path().expect("path");
    let text = std::fs::read_to_string(&path).expect("file written");
    let json: serde_json::Value = serde_json::from_str(&text).expect("valid json");

    assert_eq!(json["schemaVersion"], 1);
    assert!(json["generatedAt"].is_string());
    let providers = json["providers"].as_array().expect("providers array");
    let claude = providers
        .iter()
        .find(|p| p["provider"] == "claude")
        .expect("claude record");
    assert_eq!(claude["label"], "Claude");
    assert_eq!(claude["source"], "oauth");
    assert_eq!(claude["state"]["status"], "fresh");
    assert_eq!(claude["state"]["stale"], false);
    assert!(claude["state"]["refreshedAt"].is_string());

    // Window identities must match quota-axi's Claude scheme exactly, or the
    // tool rejects the record.
    let windows = claude["windows"].as_array().expect("windows");
    let five = windows.iter().find(|w| w["id"] == "five_hour").unwrap();
    assert_eq!(five["label"], "session");
    assert_eq!(five["kind"], "session");
    assert_eq!(five["windowSeconds"], 18_000);
    assert_eq!(five["percentUsed"], 42.0);
    assert_eq!(five["percentRemaining"], 58.0);

    let seven = windows.iter().find(|w| w["id"] == "seven_day").unwrap();
    assert_eq!(seven["label"], "week");
    assert_eq!(seven["kind"], "weekly");
    assert_eq!(seven["windowSeconds"], 604_800);

    let opus = windows.iter().find(|w| w["id"] == "seven_day_opus").unwrap();
    assert_eq!(opus["kind"], "model");

    let fable = windows.iter().find(|w| w["id"] == "model:fable").unwrap();
    assert_eq!(fable["label"], "Fable week");
    assert_eq!(fable["kind"], "model");
}

#[test]
fn reads_real_quota_axi_written_record() {
    let _env = CacheEnv::new();
    // Byte-for-byte the shape quota-axi writes (its Fable example), to prove
    // jcode consumes what the tool produces without a parallel format.
    let path = cache_file_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let refreshed = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = format!(
        r#"{{
  "generatedAt": "{refreshed}",
  "schemaVersion": 1,
  "providers": [
    {{
      "provider": "claude",
      "label": "Claude",
      "source": "oauth",
      "windows": [
        {{ "id": "five_hour", "label": "session", "kind": "session", "percentUsed": 10, "percentRemaining": 90, "windowSeconds": 18000, "resetsAt": "2999-01-01T00:00:00Z" }},
        {{ "id": "model:fable", "label": "Fable week", "kind": "model", "percentUsed": 20, "percentRemaining": 80, "windowSeconds": 604800 }}
      ],
      "state": {{ "status": "fresh", "stale": false, "refreshedAt": "{refreshed}", "sourcesTried": ["oauth"] }}
    }}
  ]
}}
"#
    );
    std::fs::write(&path, body).unwrap();

    let read = read_anthropic().expect("record parsed");
    assert!((read.five_hour - 0.10).abs() < 0.01);
    assert_eq!(read.model_scoped.len(), 1);
    assert_eq!(read.model_scoped[0].model_name, "Fable");
}

#[test]
fn stale_record_is_ignored_on_read() {
    let _env = CacheEnv::new();
    let path = cache_file_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // refreshedAt older than the TTL horizon.
    let old = (chrono::Utc::now() - chrono::Duration::seconds(SHARED_CACHE_TTL_SECS + 60))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = format!(
        r#"{{"generatedAt":"{old}","schemaVersion":1,"providers":[{{"provider":"claude","label":"Claude","source":"oauth","windows":[{{"id":"five_hour","label":"session","kind":"session","percentUsed":10,"windowSeconds":18000}}],"state":{{"status":"fresh","stale":false,"refreshedAt":"{old}","sourcesTried":["oauth"]}}}}]}}"#
    );
    std::fs::write(&path, body).unwrap();
    assert!(read_anthropic().is_none(), "stale record must not be served");
}

#[test]
fn error_usage_is_not_written_so_backoff_stays_l1() {
    let _env = CacheEnv::new();
    let errored = UsageData {
        last_error: Some("Usage API error (429): rate limit".to_string()),
        fetched_at: Some(Instant::now()),
        ..Default::default()
    };
    write_anthropic(&errored);
    assert!(
        cache_file_path().map(|p| !p.exists()).unwrap_or(true),
        "error state must never reach the shared file"
    );
    assert!(read_anthropic().is_none());
}

#[test]
fn wrong_schema_version_is_ignored() {
    let _env = CacheEnv::new();
    let path = cache_file_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"generatedAt":"x","schemaVersion":2,"providers":[]}"#).unwrap();
    assert!(read_anthropic().is_none());
}

#[test]
fn openai_write_then_read_round_trips() {
    let _env = CacheEnv::new();
    let data = OpenAIUsageData {
        five_hour: Some(OpenAIUsageWindow {
            name: "5-hour window".to_string(),
            usage_ratio: 0.6,
            resets_at: Some("2999-01-01T00:00:00Z".to_string()),
        }),
        seven_day: Some(OpenAIUsageWindow {
            name: "7-day window".to_string(),
            usage_ratio: 0.25,
            resets_at: Some("2999-01-08T00:00:00Z".to_string()),
        }),
        spark: Some(OpenAIUsageWindow {
            name: "spark".to_string(),
            usage_ratio: 0.9,
            resets_at: None,
        }),
        hard_limit_reached: false,
        fetched_at: Some(Instant::now()),
        last_error: None,
    };
    write_openai(&data);

    let path = cache_file_path().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let codex = json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "codex")
        .expect("codex record");
    let windows = codex["windows"].as_array().unwrap();
    // Only the two windows with valid quota-axi codex identities are written;
    // spark has no codex identity and stays L1-only.
    assert_eq!(windows.len(), 2);
    let five = windows.iter().find(|w| w["id"] == "five_hour").unwrap();
    assert_eq!(five["label"], "session");
    assert_eq!(five["kind"], "session");
    let seven = windows.iter().find(|w| w["id"] == "seven_day").unwrap();
    assert_eq!(seven["label"], "week");
    assert_eq!(seven["kind"], "weekly");

    let read = read_openai().expect("record parsed");
    assert!((read.five_hour.unwrap().usage_ratio - 0.6).abs() < 0.01);
    assert!((read.seven_day.unwrap().usage_ratio - 0.25).abs() < 0.01);
    assert!(read.spark.is_none());
}

#[test]
fn upsert_preserves_other_providers_and_ordering() {
    let _env = CacheEnv::new();
    // Seed a codex record, then write claude; both must survive and be ordered
    // per quota-axi's PROVIDER_IDS (claude before codex).
    write_openai(&OpenAIUsageData {
        five_hour: Some(OpenAIUsageWindow {
            name: "5-hour window".to_string(),
            usage_ratio: 0.1,
            resets_at: None,
        }),
        fetched_at: Some(Instant::now()),
        ..Default::default()
    });
    write_anthropic(&sample_usage());

    let path = cache_file_path().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let providers = json["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0]["provider"], "claude");
    assert_eq!(providers[1]["provider"], "codex");
}

#[test]
fn slugify_matches_quota_axi() {
    assert_eq!(slugify("Fable"), "fable");
    assert_eq!(slugify("Claude Opus 4.6"), "claude_opus_4_6");
    assert_eq!(slugify("  Trailing  "), "trailing");
}

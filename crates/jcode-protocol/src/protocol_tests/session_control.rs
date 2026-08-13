// Wire-stability tests for the session account-switch control surface
// (ADR 0031). These lock down the JSON shape an external orchestrator
// (quota-axi) depends on, so a serde rename or field change is caught here.

#[test]
fn test_list_sessions_request_roundtrip() -> Result<()> {
    let req = Request::ListSessions { id: 5 };
    let json = serde_json::to_string(&req)?;
    assert!(json.contains("\"type\":\"list_sessions\""));
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 5);
    assert!(matches!(decoded, Request::ListSessions { id: 5 }));
    assert!(req.is_lightweight_control_request());
    Ok(())
}

#[test]
fn test_switch_session_account_request_roundtrip() -> Result<()> {
    let req = Request::SwitchSessionAccount {
        id: 9,
        session_id: Some("sess_1".to_string()),
        account: "claude-2".to_string(),
    };
    let json = serde_json::to_string(&req)?;
    assert!(json.contains("\"type\":\"switch_session_account\""));
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 9);
    let Request::SwitchSessionAccount {
        session_id,
        account,
        ..
    } = decoded
    else {
        return Err(anyhow!("expected SwitchSessionAccount request"));
    };
    assert_eq!(session_id.as_deref(), Some("sess_1"));
    assert_eq!(account, "claude-2");
    assert!(req.is_lightweight_control_request());
    Ok(())
}

#[test]
fn test_switch_session_account_all_sessions_omits_session_id() -> Result<()> {
    // `session_id: None` (switch every live session) must serialize without the
    // field so an all-sessions request is unambiguous on the wire.
    let req = Request::SwitchSessionAccount {
        id: 1,
        session_id: None,
        account: "openai-2".to_string(),
    };
    let json = serde_json::to_string(&req)?;
    assert!(!json.contains("session_id"), "unexpected session_id in {json}");
    Ok(())
}

#[test]
fn test_switch_session_account_model_request_roundtrip() -> Result<()> {
    let req = Request::SwitchSessionAccountModel {
        id: 12,
        session_id: None,
        account: "claude-2".to_string(),
        model: "claude-api:claude-fable-5".to_string(),
    };
    let json = serde_json::to_string(&req)?;
    assert!(json.contains("\"type\":\"switch_session_account_model\""));
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 12);
    let Request::SwitchSessionAccountModel {
        session_id,
        account,
        model,
        ..
    } = decoded
    else {
        return Err(anyhow!("expected SwitchSessionAccountModel request"));
    };
    assert_eq!(session_id, None);
    assert_eq!(account, "claude-2");
    assert_eq!(model, "claude-api:claude-fable-5");
    assert!(req.is_lightweight_control_request());
    Ok(())
}

#[test]
fn test_session_list_event_roundtrip() -> Result<()> {
    let event = ServerEvent::SessionList {
        id: 5,
        sessions: vec![SessionControlInfo {
            session_id: "sess_1".to_string(),
            friendly_name: Some("fox".to_string()),
            provider: Some("Claude".to_string()),
            account: Some("claude-2".to_string()),
            model: Some("claude-opus-5".to_string()),
            is_processing: true,
        }],
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"session_list\""));
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::SessionList { id, sessions } = decoded else {
        return Err(anyhow!("expected SessionList event"));
    };
    assert_eq!(id, 5);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "sess_1");
    assert_eq!(sessions[0].account.as_deref(), Some("claude-2"));
    assert!(sessions[0].is_processing);
    Ok(())
}

#[test]
fn test_session_switch_result_event_roundtrip() -> Result<()> {
    let event = ServerEvent::SessionSwitchResult {
        id: 9,
        results: vec![
            SessionSwitchOutcome {
                session_id: "sess_1".to_string(),
                ok: true,
                account: Some("claude-2".to_string()),
                model: Some("claude-opus-5".to_string()),
                deferred: true,
                error: None,
            },
            SessionSwitchOutcome {
                session_id: "sess_2".to_string(),
                ok: false,
                account: Some("claude-2".to_string()),
                model: None,
                deferred: false,
                error: Some("no account 'claude-2'".to_string()),
            },
        ],
    };
    let json = encode_event(&event);
    assert!(json.contains("\"type\":\"session_switch_result\""));
    let decoded = parse_event_json(json.trim())?;
    let ServerEvent::SessionSwitchResult { id, results } = decoded else {
        return Err(anyhow!("expected SessionSwitchResult event"));
    };
    assert_eq!(id, 9);
    assert_eq!(results.len(), 2);
    assert!(results[0].ok && results[0].deferred);
    assert!(!results[1].ok);
    assert_eq!(results[1].error.as_deref(), Some("no account 'claude-2'"));
    Ok(())
}

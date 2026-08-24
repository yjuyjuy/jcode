use super::*;

#[test]
fn request_model_matches_plain_and_prefixed_and_pinned() {
    // Exact match (the common fleet case: a plain id in, a plain id back).
    assert!(request_model_matches(
        "deepseek-v4-flash",
        "deepseek-v4-flash"
    ));
    // A route prefix on the request is consumed; the applied id is bare.
    assert!(request_model_matches(
        "claude-api:claude-fable-5",
        "claude-fable-5"
    ));
    // An explicit @pin on the applied model still matches the bare request.
    assert!(request_model_matches("z-ai/glm-5.2", "z-ai/glm-5.2@Novita"));
    // Case-insensitive.
    assert!(request_model_matches("Claude-Fable-5", "claude-fable-5"));
    // OpenRouter vendor/model ids (which contain '/') are not mistaken for a
    // route prefix.
    assert!(request_model_matches("z-ai/glm-5.2", "z-ai/glm-5.2"));
}

#[test]
fn request_model_matches_rejects_a_different_model() {
    // The whole point: a genuinely different applied model must NOT verify, so a
    // silent alias/no-op is caught loudly.
    assert!(!request_model_matches(
        "deepseek-v4-flash",
        "claude-opus-4-6"
    ));
    assert!(!request_model_matches(
        "claude-api:claude-fable-5",
        "claude-opus-4-6"
    ));
}

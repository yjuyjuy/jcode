//! Overscroll status-strip helpers, split out of `ui_input.rs` to keep that
//! file within the code-size budget. These format the individual facts shown
//! in the scroll-region status line (provider, auth, model, context usage,
//! directory, git branch) and truncate the assembled span list to fit.

use crate::tui::color_support::rgb;
use crate::tui::session_facts;
use ratatui::prelude::*;

/// Truncate a list of spans to at most `max_width` display columns, appending a
/// single-cell ellipsis when content is dropped. Preserves per-span styling.
pub(crate) fn overscroll_truncate_spans(
    spans: Vec<Span<'static>>,
    max_width: usize,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    if max_width == 0 {
        return Vec::new();
    }
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    if total <= max_width {
        return spans;
    }
    // Leave room for a trailing ellipsis.
    let budget = max_width.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = span.content.width();
        if used + w <= budget {
            used += w;
            out.push(span);
            continue;
        }
        // Partial: take as many chars as fit within the remaining budget.
        let remaining = budget - used;
        if remaining > 0 {
            let mut taken = String::new();
            let mut tw = 0usize;
            for ch in span.content.chars() {
                let cw = UnicodeWidthStr::width(ch.to_string().as_str());
                if tw + cw > remaining {
                    break;
                }
                tw += cw;
                taken.push(ch);
            }
            if !taken.is_empty() {
                out.push(Span::styled(taken, span.style));
            }
        }
        break;
    }
    out.push(Span::styled("…", Style::default().fg(rgb(100, 100, 110))));
    out
}

/// Format a working dir path home-relative (~/foo/bar), keeping the last 2 segments.
/// Compact git branch label for the status line and fact stack. Truncated so
/// long branch names cannot crowd out the other facts.
pub(crate) fn overscroll_git_branch(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<String> {
    let branch = data.git_info.as_ref()?.branch.trim();
    if branch.is_empty() {
        return None;
    }
    let mut label: String = branch.chars().take(24).collect();
    if branch.chars().count() > 24 {
        label.push('…');
    }
    Some(label)
}

pub(crate) fn overscroll_dir_label(path: &str) -> Option<String> {
    session_facts::dir_label_short(path)
}

/// Placeholder header strings used during remote startup; not real model names.
pub(crate) fn overscroll_is_placeholder(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("connecting")
        || m.starts_with("loading")
        || m == "connected"
        || m.contains("connecting to server")
}

/// Inert runtime provider labels (remote/replay/test-harness) shown before the
/// real provider name is known; not real provider names.
pub(crate) fn overscroll_is_runtime_placeholder(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "remote" | "replay" | "test-harness"
    )
}

pub(crate) fn overscroll_provider_display(provider: &str) -> String {
    match provider.to_ascii_lowercase().as_str() {
        // Keep provider labels credential-neutral: the adjacent auth chip
        // (`overscroll_auth_label`) reports OAuth vs API key from the canonical
        // credential resolution. Baking a credential into the provider name
        // used to produce contradictions like "Claude OAuth · API key" when
        // the Anthropic route was pinned to the API key.
        "claude" => "Claude".to_string(),
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        "opencode" => "OpenCode".to_string(),
        "gemini" => "Gemini".to_string(),
        "copilot" => "GitHub Copilot".to_string(),
        "cursor" => "Cursor".to_string(),
        "bedrock" => "AWS Bedrock".to_string(),
        "antigravity" => "Antigravity".to_string(),
        _ => provider.to_string(),
    }
}

pub(crate) fn overscroll_auth_label(
    method: crate::tui::info_widget::AuthMethod,
) -> Option<(&'static str, Color)> {
    use crate::tui::info_widget::AuthMethod;
    match method {
        AuthMethod::Unknown => None,
        AuthMethod::ApiKey | AuthMethod::AnthropicApiKey | AuthMethod::OpenAIApiKey => {
            Some(("API key", rgb(180, 180, 190)))
        }
        AuthMethod::OpenRouterApiKey | AuthMethod::OpenCodeApiKey => {
            Some(("API key", rgb(140, 180, 255)))
        }
        AuthMethod::AnthropicOAuth => Some(("OAuth", rgb(255, 160, 100))),
        AuthMethod::OpenAIOAuth => Some(("OAuth", rgb(100, 200, 180))),
        AuthMethod::CopilotOAuth => Some(("OAuth", rgb(110, 200, 140))),
        AuthMethod::GeminiOAuth => Some(("OAuth", rgb(120, 190, 255))),
    }
}

pub(crate) fn overscroll_short_reasoning(effort: &str) -> Option<&str> {
    let effort = effort.trim();
    if effort.is_empty() {
        return None;
    }
    Some(match effort {
        "max" => "max",
        "xhigh" => "xhigh",
        "high" => "high",
        "medium" => "medium",
        "low" => "low",
        "none" => "none",
        other => other,
    })
}

pub(crate) fn overscroll_context_usage(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<(usize, usize)> {
    let used_tokens = if let Some(observed) = data.observed_context_tokens {
        observed as usize
    } else {
        let info = data.context_info.as_ref()?;
        if info.total_chars == 0 {
            return None;
        }
        info.estimated_tokens()
    };
    let limit = data
        .context_limit
        .unwrap_or(crate::provider::DEFAULT_CONTEXT_LIMIT)
        .max(1);
    Some((used_tokens, limit))
}

/// Format a token count compactly: 1234 -> "1.2k", 200000 -> "200k", 1_500_000 -> "1.5M".
pub(crate) fn overscroll_format_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{}k", tokens / 1000)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// Render a compact rounded progress bar (◖████░░◗) plus a percentage label.
pub(crate) fn overscroll_context_bar(
    used: usize,
    limit: usize,
    cells: usize,
) -> Vec<Span<'static>> {
    let limit = limit.max(1);
    let ratio = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let pct = (ratio * 100.0).round() as u16;
    let filled = (ratio * cells as f64).round() as usize;
    let filled = filled.min(cells);

    // Match the info widget usage bar palette (based on remaining context).
    let left_pct = 100u16.saturating_sub(pct);
    let fill_color = if left_pct <= 20 {
        rgb(255, 100, 100)
    } else if left_pct <= 50 {
        rgb(255, 200, 100)
    } else {
        rgb(100, 200, 100)
    };
    let track_color = rgb(50, 50, 60);

    let mut spans = Vec::with_capacity(cells + 2);
    // Slim segmented pill (▰ filled / ▱ empty) reads thinner than full blocks.
    spans.push(Span::styled(
        "▰".repeat(filled),
        Style::default().fg(fill_color),
    ));
    spans.push(Span::styled(
        "▱".repeat(cells.saturating_sub(filled)),
        Style::default().fg(track_color),
    ));
    spans.push(Span::styled(
        format!(" {}%", pct),
        Style::default().fg(fill_color).bold(),
    ));
    spans
}

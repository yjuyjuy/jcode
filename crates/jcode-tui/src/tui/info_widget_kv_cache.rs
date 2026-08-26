//! KV-cache widget rendering, split out of `info_widget.rs` to keep that file
//! within the code-size budget. Renders the cache hit/miss summary and its
//! per-turn miss attribution, plus the small formatting helpers they share.

use super::super::color_support::rgb;
use super::{CacheHitInfo, InfoWidgetData};
use ratatui::{prelude::*, style::Modifier};

pub(crate) fn render_kv_cache_widget(data: &InfoWidgetData, _inner: Rect) -> Vec<Line<'static>> {
    let Some(cache) = data.cache_hit_info.as_ref() else {
        return Vec::new();
    };
    let mut lines = vec![render_kv_cache_summary_line(cache)];

    lines.push(Line::from(vec![Span::styled(
        "miss attribution",
        Style::default().fg(rgb(140, 140, 150)).bold(),
    )]));

    if cache.miss_attributions.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "none",
            Style::default().fg(rgb(110, 210, 140)),
        )]));
        return lines;
    }

    let total_missed: u64 = cache
        .miss_attributions
        .iter()
        .map(|sample| sample.missed_tokens)
        .sum();
    lines.push(Line::from(vec![Span::styled(
        format!("{} missed total", compact_token_count(total_missed)),
        Style::default().fg(rgb(180, 180, 190)),
    )]));

    for sample in cache.miss_attributions.iter().take(5) {
        lines.push(Line::from(vec![
            Span::styled(
                format_cache_turn_label(sample.turn_number, sample.call_index),
                Style::default().fg(rgb(140, 180, 255)).bold(),
            ),
            Span::styled(
                format!(" {} miss ", compact_token_count(sample.missed_tokens)),
                Style::default().fg(rgb(255, 200, 100)),
            ),
            Span::styled(
                format!("({})", sample.reason),
                Style::default().fg(rgb(140, 140, 150)),
            ),
        ]));
    }

    if cache.miss_attributions.len() > 5 {
        lines.push(Line::from(vec![Span::styled(
            format!("… {} more", cache.miss_attributions.len() - 5),
            Style::default().fg(rgb(100, 100, 110)),
        )]));
    }

    lines
}

pub(crate) fn render_kv_cache_summary_line(cache: &CacheHitInfo) -> Line<'static> {
    let Some(lifetime_ratio) = cache.hit_ratio() else {
        return Line::default();
    };

    let lifetime_pct = ratio_pct(lifetime_ratio);
    let warm_pct = cache.optimal_ratio().map(ratio_pct);
    let last_pct = cache.last_ratio().map(ratio_pct);
    let last_optimal_pct = cache.last_optimal_ratio().map(ratio_pct);
    let health_pct = last_optimal_pct
        .or(last_pct)
        .or(warm_pct)
        .unwrap_or(lifetime_pct);
    let color = kv_cache_optimal_color(health_pct);

    let mut spans = vec![Span::styled(
        "KV cache: ",
        Style::default().fg(rgb(180, 180, 190)).bold(),
    )];

    if let Some(warm_pct) = warm_pct {
        spans.push(Span::styled(
            "yield ",
            Style::default().fg(rgb(140, 140, 150)),
        ));
        spans.push(Span::styled(
            format!("{}%", warm_pct),
            Style::default().fg(color).bold(),
        ));
    } else {
        spans.push(Span::styled(
            "priming",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(last_pct) = last_pct {
        spans.push(Span::styled(" · ", Style::default().fg(rgb(80, 80, 90))));
        spans.push(Span::styled(
            "last ",
            Style::default().fg(rgb(140, 140, 150)),
        ));
        spans.push(Span::styled(
            format!("{}%", last_pct),
            Style::default().fg(color).bold(),
        ));
    }

    spans.push(Span::styled(" · ", Style::default().fg(rgb(80, 80, 90))));
    spans.push(Span::styled(
        "session ",
        Style::default().fg(rgb(140, 140, 150)),
    ));
    spans.push(Span::styled(
        format!("{}%", lifetime_pct),
        Style::default().fg(color).bold(),
    ));

    Line::from(spans)
}

fn ratio_pct(ratio: f32) -> u8 {
    (ratio * 100.0).round().clamp(0.0, 100.0) as u8
}

fn kv_cache_optimal_color(pct: u8) -> Color {
    match pct {
        0..=24 => rgb(255, 110, 110),
        25..=59 => rgb(255, 200, 100),
        60..=84 => rgb(140, 180, 255),
        _ => rgb(110, 210, 140),
    }
}

fn format_cache_turn_label(turn_number: usize, call_index: u16) -> String {
    if call_index <= 1 {
        format!("{}>", turn_number)
    } else {
        format!("{}.{}>", turn_number, call_index)
    }
}

fn compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f32 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f32 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

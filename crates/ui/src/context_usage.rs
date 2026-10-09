//! Context occupancy is read from the replicated chat snapshot, never local CLI state.
use crate::theme::Theme;
use gpui::{IntoElement, PathBuilder, SharedString, canvas, div, point, prelude::*, px};
use zeron_proto::ContextUsage;

/// The context ring's trigger chip; the footer ([`crate::account_usage`])
/// opens [`card`] from it on click.
pub fn chip(
    usage: Option<ContextUsage>,
    open: bool,
    compact: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let fraction = usage.and_then(ContextUsage::fraction);
    let color = match fraction {
        Some(f) if f >= 0.9 => theme.danger,
        Some(f) if f >= 0.75 => theme.warning,
        Some(_) => theme.text_muted,
        None => theme.text_faint,
    };
    let label = if compact {
        String::new()
    } else {
        fraction
            .map(|f| format!("{:.0}%", f * 100.0))
            .unwrap_or_else(|| "—".into())
    };
    ring_chip(
        "context-usage",
        fraction.unwrap_or(0.0) as f32,
        color,
        color,
        label,
        open,
        theme,
    )
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn detailed_label(usage: Option<ContextUsage>) -> String {
    let usage = usage.unwrap_or_default();
    let tokens = usage.tokens.map(compact_tokens);
    let window = usage.window.filter(|size| *size > 0).map(compact_tokens);
    match (tokens, window, usage.fraction()) {
        (Some(tokens), Some(window), Some(fraction)) => {
            format!("{tokens}/{window} ({:.0}%)", fraction * 100.0)
        }
        (Some(tokens), _, _) => format!("{tokens} / limit unavailable"),
        (_, Some(window), _) => format!("Unavailable / {window}"),
        _ => "Unavailable".into(),
    }
}

pub(crate) fn detailed_chip(
    usage: Option<ContextUsage>,
    open: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let fraction = usage.and_then(ContextUsage::fraction);
    let color = match fraction {
        Some(fraction) if fraction >= 0.9 => theme.danger,
        Some(fraction) if fraction >= 0.75 => theme.warning,
        _ => theme.text_muted,
    };
    let mut bar = div()
        .w(px(80.0))
        .h(px(10.0))
        .flex_none()
        .flex()
        .gap(px(2.0));
    for segment in 0..20 {
        let fill = (fraction.unwrap_or(0.0).clamp(0.0, 1.0) * 20.0 - segment as f64).clamp(0.0, 1.0)
            as f32;
        bar = bar.child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(theme.text_faint.opacity(0.25))
                .when(fill > 0.0, |segment| {
                    segment.child(div().h_full().w(gpui::relative(fill)).bg(color))
                }),
        );
    }
    div()
        .id("context-usage-detailed")
        .min_w_0()
        .max_w_full()
        .min_h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .text_size(px(11.0))
        .line_height(px(24.0))
        .text_color(color)
        .cursor_pointer()
        .when(open, |chip| chip.bg(crate::theme::ink(0.05)))
        .hover(|chip| chip.bg(crate::theme::ink(0.05)))
        .child("Context")
        .child(bar)
        .child(detailed_label(usage))
}

/// Account and context rings retain the same hit target as the TPS chip.
pub(crate) fn ring_chip(
    id: &'static str,
    fraction: f32,
    arc: gpui::Hsla,
    text: gpui::Hsla,
    label: String,
    open: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .when(open, |s| s.bg(crate::theme::ink(0.05)))
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(ring(fraction, arc, theme))
        .when(!label.is_empty(), |chip| {
            chip.child(SharedString::from(label))
        })
}

/// The original 16px progress ring, filled clockwise from twelve o'clock.
fn ring(fraction: f32, color: gpui::Hsla, theme: &Theme) -> impl IntoElement {
    let track = theme.text_faint.opacity(0.25);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let mut arc = |fraction: f32, color| {
                if fraction <= 0.0 {
                    return;
                }
                let steps = (64.0 * fraction).ceil().max(2.0) as usize;
                let mut path = PathBuilder::stroke(px(1.8));
                for i in 0..=steps {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
                    let p = point(
                        center.x + px(6.0 * angle.cos()),
                        center.y + px(6.0 * angle.sin()),
                    );
                    if i == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            arc(1.0, track);
            arc(fraction.clamp(0.0, 1.0), color);
        },
    )
    .size(px(16.0))
}

/// One footer indicator: icon + reading, identical geometry for every
/// chip so they sit side by side as equals. `icon_color` colours the glyph,
/// `text` the label; `open` holds the hover wash while its popover is up.
pub(crate) fn icon_chip(
    id: &'static str,
    icon_path: &'static str,
    icon_color: gpui::Hsla,
    text: gpui::Hsla,
    label: String,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .when(open, |s| s.bg(crate::theme::ink(0.05)))
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(
            crate::icons::icon(icon_path)
                .size(px(16.0))
                .flex_none()
                .text_color(icon_color),
        )
        .when(!label.is_empty(), |chip| {
            chip.child(SharedString::from(label))
        })
}

pub(crate) fn with_separators(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// whether the indicator has anything to measure against: harnesses that
/// never report a window (antigravity) get no indicator at all, rather than a
/// permanently empty ring.
pub fn has_window(usage: Option<ContextUsage>) -> bool {
    usage
        .and_then(|usage| usage.window)
        .is_some_and(|window| window > 0)
}

fn details(usage: Option<ContextUsage>) -> String {
    match usage.unwrap_or_default() {
        ContextUsage {
            tokens: Some(tokens),
            window: Some(window),
        } if window > 0 => {
            format!(
                "{} / {} tokens\n{} tokens remaining",
                with_separators(tokens),
                with_separators(window),
                with_separators(window.saturating_sub(tokens))
            )
        }
        ContextUsage {
            tokens: Some(tokens),
            ..
        } => format!(
            "{} tokens used\nContext limit not reported",
            with_separators(tokens)
        ),
        ContextUsage {
            window: Some(window),
            ..
        } if window > 0 => format!(
            "{} token capacity\nWaiting for context usage",
            with_separators(window)
        ),
        _ => "Context usage not reported by this harness yet".into(),
    }
}

/// The context ring's popover content.
pub fn card(usage: Option<ContextUsage>, theme: &Theme) -> gpui::Div {
    crate::popover::popover_card(theme)
        .flex()
        .flex_col()
        .child(crate::popover::menu_heading(theme, "Context window"))
        .child(
            div()
                .px(px(8.0))
                .pb(px(6.0))
                .text_size(px(12.0))
                .line_height(px(19.0))
                .whitespace_nowrap()
                .text_color(theme.text_muted)
                .child(SharedString::from(details(usage))),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detailed_context_uses_reported_capacity_and_distinguishes_missing_data() {
        assert_eq!(
            detailed_label(Some(ContextUsage {
                tokens: Some(110_000),
                window: Some(1_000_000),
            })),
            "110k/1.0M (11%)"
        );
        assert_eq!(
            detailed_label(Some(ContextUsage {
                tokens: Some(0),
                window: Some(256_000),
            })),
            "0/256k (0%)"
        );
        assert_eq!(detailed_label(None), "Unavailable");
        assert_eq!(
            detailed_label(Some(ContextUsage {
                tokens: None,
                window: Some(1_000_000),
            })),
            "Unavailable / 1.0M"
        );
        assert_eq!(
            detailed_label(Some(ContextUsage {
                tokens: Some(12_000),
                window: Some(0),
            })),
            "12k / limit unavailable"
        );
        assert_eq!(
            detailed_label(Some(ContextUsage {
                tokens: Some(300_000),
                window: Some(200_000),
            })),
            "300k/200k (150%)"
        );
    }

    #[test]
    fn indicator_needs_a_reported_window() {
        assert!(!has_window(None));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: None,
        })));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: Some(0),
        })));
        assert!(has_window(Some(ContextUsage {
            tokens: None,
            window: Some(200_000),
        })));
    }

    #[test]
    fn missing_usage_is_distinct_from_zero_and_overflow() {
        assert!(details(None).contains("not reported"));
        assert!(
            details(Some(ContextUsage {
                tokens: Some(0),
                window: Some(200)
            }))
            .contains("200 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(250),
                window: Some(200)
            }))
            .contains("0 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(10),
                window: Some(0)
            }))
            .contains("limit not reported")
        );
    }

    #[test]
    fn token_counts_are_grouped_by_thousands() {
        assert_eq!(with_separators(0), "0");
        assert_eq!(with_separators(999), "999");
        assert_eq!(with_separators(5417), "5,417");
        assert_eq!(with_separators(1_048_576), "1,048,576");
        assert_eq!(
            details(Some(ContextUsage {
                tokens: Some(5417),
                window: Some(1_048_576)
            })),
            "5,417 / 1,048,576 tokens\n1,043,159 tokens remaining"
        );
    }
}

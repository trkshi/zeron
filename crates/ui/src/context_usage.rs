//! Context occupancy is read from the replicated chat snapshot, never local CLI state.
use crate::theme::Theme;
use gpui::{SharedString, div, prelude::*, px};
use zeron_proto::ContextUsage;

/// The context trigger chip; the footer ([`crate::account_usage`])
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
    icon_chip(
        "context-usage",
        crate::icons::CPU,
        color,
        color,
        label,
        open,
    )
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

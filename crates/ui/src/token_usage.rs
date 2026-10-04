//! Last-turn statistics come from the transcript, not provider files on the UI device.
use gpui::{SharedString, div, prelude::*, px};
use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_proto::TokenUsage;

use crate::{context_usage::with_separators, popover, theme::Theme};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TurnStats {
    usage: Option<TokenUsage>,
    duration_ms: Option<i64>,
    measuring: bool,
}

impl TurnStats {
    pub fn from_transcript(entries: &[SessionMessageEntry], measuring: bool) -> Self {
        if measuring {
            return Self {
                measuring,
                ..Default::default()
            };
        }
        // Late asynchronous questions are standalone assistant entries, not
        // new measured turns. Do not let one hide the completed turn's stats.
        entries
            .iter()
            .rev()
            .find(|entry| {
                let standalone_question = entry.duration_ms.is_none()
                    && entry.token_usage.is_none()
                    && !entry.parts.is_empty()
                    && entry.parts.iter().all(|part| {
                        matches!(
                            part,
                            MessagePart::Input {
                                asynchronous: true,
                                ..
                            }
                        )
                    });
                entry.role == MessageRole::Assistant && !standalone_question
            })
            .map(|entry| Self {
                usage: entry.token_usage.as_deref().copied(),
                duration_ms: entry.duration_ms,
                measuring: false,
            })
            .unwrap_or_default()
    }

    pub fn label(self) -> String {
        if self.measuring {
            return "Measuring".into();
        }
        self.usage
            .and_then(|usage| usage.average_tps(self.duration_ms))
            .map(|tps| format!("{tps:.1} tok/s"))
            .unwrap_or_else(|| "- tok/s".into())
    }
}

fn rows(stats: TurnStats) -> [(&'static str, String); 9] {
    let usage = stats.usage.unwrap_or_default();
    let count = |value: Option<u64>| {
        value
            .map(with_separators)
            .unwrap_or_else(|| "Not reported".into())
    };
    [
        ("Input (incl. cache)", count(usage.input_tokens)),
        ("Output", count(usage.output_tokens)),
        ("Total", count(usage.total())),
        ("Cached input", count(usage.cached_input_tokens)),
        (
            "Cache hit rate",
            usage
                .cache_hit_rate()
                .map(|rate| format!("{:.1}%", rate * 100.0))
                .unwrap_or_else(|| "Not reported".into()),
        ),
        ("Cache writes", count(usage.cache_write_input_tokens)),
        (
            "Reasoning (in output)",
            count(usage.reasoning_output_tokens),
        ),
        (
            "Average TPS",
            if stats.measuring {
                "Measuring".into()
            } else {
                usage
                    .average_tps(stats.duration_ms)
                    .map(|tps| format!("{tps:.1} tok/s"))
                    .unwrap_or_else(|| "Not reported".into())
            },
        ),
        (
            "Reported cost",
            usage
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .map(|cost| format!("${cost:.4}"))
                .unwrap_or_else(|| "Not reported".into()),
        ),
    ]
}

pub(crate) fn card(stats: TurnStats, theme: &Theme) -> gpui::Div {
    popover::popover_card(theme)
        .w(px(320.0))
        .flex()
        .flex_col()
        .child(popover::menu_heading(theme, "Token usage"))
        .child(
            div()
                .px(px(8.0))
                .pb(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(if stats.measuring {
                    "Current turn"
                } else {
                    "Last turn"
                }),
        )
        .children(rows(stats).into_iter().map(|(label, value)| {
            div()
                .flex()
                .items_start()
                .gap(px(12.0))
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.text)
                        .child(SharedString::from(value)),
                )
        }))
        .child(
            div()
                .px(px(8.0))
                .pt(px(8.0))
                .pb(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child("Average TPS includes tools and waiting."),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(usage: Option<TokenUsage>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "turn".into(),
            role: MessageRole::Assistant,
            parts: vec![],
            created_at: 1,
            device_id: "host".into(),
            status: Some(zeron_doc::MessageStatus::Complete),
            continuation_of: None,
            duration_ms: Some(20_000),
            token_usage: usage.map(Box::new),
        }
    }

    #[test]
    fn measuring_does_not_reuse_previous_turn() {
        let entries = [assistant(Some(TokenUsage {
            output_tokens: Some(400),
            ..Default::default()
        }))];
        let stats = TurnStats::from_transcript(&entries, true);
        assert_eq!(stats.label(), "Measuring");
        assert_eq!(rows(stats)[1].1, "Not reported");
        assert_eq!(
            TurnStats::from_transcript(&entries, false).label(),
            "20.0 tok/s"
        );
    }

    #[test]
    fn latest_missing_usage_does_not_fall_back_to_an_older_turn() {
        for duration in [None, Some(20_000)] {
            let mut latest = assistant(None);
            latest.duration_ms = duration;
            let entries = [
                assistant(Some(TokenUsage {
                    output_tokens: Some(400),
                    ..Default::default()
                })),
                latest,
            ];
            let stats = TurnStats::from_transcript(&entries, false);
            assert_eq!(stats.label(), "- tok/s");
            assert!(rows(stats).iter().all(|(_, value)| value == "Not reported"));
        }
    }

    #[test]
    fn a_late_question_is_not_a_new_measured_turn() {
        let mut question = assistant(None);
        question.duration_ms = None;
        question.parts.push(MessagePart::Input {
            id: "late-question-part".into(),
            request_id: "late-question".into(),
            questions: vec![],
            resolved: false,
            asynchronous: true,
        });
        let entries = [
            assistant(Some(TokenUsage {
                output_tokens: Some(400),
                ..Default::default()
            })),
            question,
        ];
        assert_eq!(
            TurnStats::from_transcript(&entries, false).label(),
            "20.0 tok/s"
        );
    }

    #[test]
    fn missing_counts_stay_distinct_from_reported_zero() {
        let stats = TurnStats::from_transcript(
            &[assistant(Some(TokenUsage {
                input_tokens: Some(1000),
                output_tokens: Some(0),
                cached_input_tokens: Some(500),
                ..Default::default()
            }))],
            false,
        );
        let rows = rows(stats);
        assert_eq!(rows[0].1, "1,000");
        assert_eq!(rows[1].1, "0");
        assert_eq!(rows[2].1, "1,000");
        assert_eq!(rows[4].1, "50.0%");
        assert_eq!(rows[5].1, "Not reported");
        assert_eq!(stats.label(), "0.0 tok/s");
    }
}

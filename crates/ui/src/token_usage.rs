//! Recorded thread totals, request generation TPS, and turn-average TPS.
use gpui::{SharedString, div, prelude::*, px};
#[cfg(test)]
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
#[cfg(test)]
use zeron_proto::{GenerationUsage, TokenUsage};

use crate::{context_usage::with_separators, popover, theme::Theme};

mod stats;

use stats::TokenStats;
pub(crate) use stats::{TokenStatsCache, invalidates_stats};

fn rows(stats: TokenStats) -> [(&'static str, String); 10] {
    let usage = stats.thread_usage;
    let count = |value: Option<u64>| {
        value
            .map(with_separators)
            .unwrap_or_else(|| "Not reported".into())
    };
    [
        ("Input (incl. cache)", count(usage.input_tokens)),
        ("Output", count(usage.output_tokens)),
        ("Total", count(usage.total_tokens)),
        ("Cached input", count(usage.cached_input_tokens)),
        (
            "Cache hit rate",
            stats
                .cache_hit_rate
                .map(|rate| format!("{:.1}%", rate * 100.0))
                .unwrap_or_else(|| "Not reported".into()),
        ),
        ("Cache writes", count(usage.cache_write_input_tokens)),
        (
            "Reasoning (in output)",
            count(usage.reasoning_output_tokens),
        ),
        (
            stats.average_description(),
            stats
                .turn_usage
                .and_then(|usage| usage.average_tps(stats.duration_ms))
                .map(|tps| format!("{tps:.1} tok/s"))
                .unwrap_or_else(|| "Not reported".into()),
        ),
        (
            "Reported cost",
            usage
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .map(|cost| format!("${cost:.4}"))
                .unwrap_or_else(|| "Not reported".into()),
        ),
        (
            stats.generation_description(),
            stats
                .generation_label()
                .unwrap_or_else(|| "Not reported".into()),
        ),
    ]
}

pub(crate) fn card(stats: TokenStats, theme: &Theme) -> gpui::Div {
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
                .child("Reported thread totals"),
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
                .child("Generation TPS excludes first-output wait and separately reported reasoning. Turn average includes tools and waiting."),
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

    fn generation(output: u64) -> GenerationUsage {
        GenerationUsage {
            output_tokens: output,
            reasoning_output_tokens: Some(40),
            elapsed_ms: 1000,
            ttft_ms: 200,
            estimated: false,
        }
    }

    #[test]
    fn saved_client_arrival_estimates_fall_back_to_turn_average() {
        let mut generation = generation(100);
        generation.estimated = true;
        let entries = [assistant(Some(TokenUsage {
            output_tokens: Some(400),
            generation: Some(generation),
            ..Default::default()
        }))];
        let stats = TokenStats::from_transcript(&entries, false);
        assert_eq!(stats.label(), "20.0 tok/s");
        assert_eq!(stats.rate_description(), "Average TPS (last turn)");
        assert_eq!(rows(stats)[9].1, "Not reported");
        assert_eq!(rows(stats)[7].1, "20.0 tok/s");
        let working = TokenStats::from_transcript(&entries, true);
        assert_eq!(working.label(), "20.0 tok/s");
        assert_eq!(
            working.rate_description(),
            "Average TPS (previous turn)"
        );
    }

    #[test]
    fn live_client_arrival_estimates_cannot_replace_average_or_server_rates() {
        let previous = assistant(Some(TokenUsage {
            output_tokens: Some(200),
            ..Default::default()
        }));
        let mut estimate = generation(100);
        estimate.estimated = true;
        let mut current = assistant(Some(TokenUsage {
            output_tokens: Some(400),
            generation: Some(estimate),
            ..Default::default()
        }));
        current.status = Some(MessageStatus::Streaming);
        let mut entries = [previous, current];
        let live = TokenStats::from_transcript(&entries, true);
        assert_eq!(live.label(), "20.0 tok/s");
        assert_eq!(live.rate_description(), "Live average TPS");
        assert_eq!(rows(live)[9].1, "Not reported");

        entries[0].token_usage.as_mut().unwrap().generation = Some(generation(100));
        let measured = TokenStats::from_transcript(&entries, true);
        assert_eq!(measured.label(), "75.0 tok/s");
        assert_eq!(measured.rate_description(), "Generation TPS (previous turn)");
        assert_eq!(rows(measured)[7].1, "20.0 tok/s");

        entries[0].token_usage.as_mut().unwrap().generation = Some(estimate);
        entries[1].token_usage.as_mut().unwrap().generation = None;
        let previous_estimate = TokenStats::from_transcript(&entries, true);
        assert_eq!(previous_estimate.label(), "20.0 tok/s");
        assert_eq!(previous_estimate.rate_description(), "Live average TPS");
        assert_eq!(rows(previous_estimate)[9].1, "Not reported");
    }

    #[test]
    fn footer_prefers_latest_request_generation_not_the_turn_average() {
        let mut entry = assistant(Some(TokenUsage {
            output_tokens: Some(400),
            generation: Some(generation(100)),
            ..Default::default()
        }));
        entry.status = Some(MessageStatus::Streaming);
        let mut entries = [entry];
        let first = TokenStats::from_transcript(&entries, true);
        assert_eq!(first.label(), "75.0 tok/s");
        assert_eq!(first.rate_description(), "Generation TPS (latest request)");
        assert_eq!(rows(first)[7].1, "20.0 tok/s");
        assert_eq!(rows(first)[9].1, "75.0 tok/s");
        entries[0].duration_ms = Some(40_000);
        entries[0].status = Some(MessageStatus::Complete);
        let done = TokenStats::from_transcript(&entries, false);
        assert_eq!(done.label(), "75.0 tok/s");
        assert_eq!(rows(done)[7].1, "10.0 tok/s");
        entries[0].token_usage.as_mut().unwrap().generation = Some(generation(200));
        assert_eq!(
            TokenStats::from_transcript(&entries, false).label(),
            "200.0 tok/s"
        );
    }

    #[test]
    fn generation_holds_previous_turn_until_this_turn_reports_a_request() {
        let previous = assistant(Some(TokenUsage {
            output_tokens: Some(400),
            generation: Some(generation(100)),
            ..Default::default()
        }));
        let mut current = assistant(Some(TokenUsage {
            output_tokens: Some(100),
            ..Default::default()
        }));
        current.status = Some(MessageStatus::Streaming);
        let mut entries = [previous, current];
        let first = TokenStats::from_transcript(&entries, true);
        assert_eq!(first.label(), "75.0 tok/s");
        assert_eq!(first.rate_description(), "Generation TPS (previous turn)");
        assert_eq!(rows(first)[7].1, "5.0 tok/s");
        entries[1].token_usage.as_mut().unwrap().generation = Some(generation(200));
        assert_eq!(
            TokenStats::from_transcript(&entries, true).label(),
            "200.0 tok/s"
        );
        entries[1].token_usage.as_mut().unwrap().generation = None;
        entries[1].status = Some(MessageStatus::Complete);
        let completed = TokenStats::from_transcript(&entries, false);
        assert_eq!(completed.label(), "5.0 tok/s");
        assert_eq!(completed.rate_description(), "Average TPS (last turn)");
        assert_eq!(rows(completed)[9].1, "Not reported");
    }

    #[test]
    fn invalid_request_windows_fall_back_to_clearly_labeled_turn_average() {
        let mut invalid = generation(100);
        invalid.ttft_ms = invalid.elapsed_ms;
        let entries = [assistant(Some(TokenUsage {
            output_tokens: Some(400),
            generation: Some(invalid),
            ..Default::default()
        }))];
        let stats = TokenStats::from_transcript(&entries, false);
        assert_eq!(stats.label(), "20.0 tok/s");
        assert_eq!(stats.rate_description(), "Average TPS (last turn)");
        assert_eq!(rows(stats)[9].1, "Not reported");
    }

    #[test]
    fn a_new_turn_keeps_previous_tps_until_output_is_reported() {
        let entries = [assistant(Some(TokenUsage {
            output_tokens: Some(400),
            ..Default::default()
        }))];
        let stats = TokenStats::from_transcript(&entries, true);
        assert_eq!(stats.label(), "20.0 tok/s");
        assert_eq!(rows(stats)[1].1, "400");
        assert_eq!(
            rows(stats)[7],
            ("Average TPS (previous turn)", "20.0 tok/s".into())
        );
        assert_eq!(
            TokenStats::from_transcript(&entries, false).label(),
            "20.0 tok/s"
        );
    }

    #[test]
    fn live_rate_replaces_the_previous_turn_when_output_is_reported() {
        let previous = assistant(Some(TokenUsage {
            output_tokens: Some(400),
            ..Default::default()
        }));
        let mut current = assistant(None);
        current.id = "current".into();
        current.status = Some(MessageStatus::Streaming);
        current.duration_ms = Some(10_000);
        let mut entries = [previous, current];
        assert_eq!(
            TokenStats::from_transcript(&entries, true).label(),
            "20.0 tok/s"
        );
        entries[1].token_usage = Some(Box::new(TokenUsage {
            output_tokens: Some(0),
            ..Default::default()
        }));
        assert_eq!(
            TokenStats::from_transcript(&entries, true).label(),
            "20.0 tok/s"
        );
        entries[1].token_usage.as_mut().unwrap().output_tokens = Some(100);
        let live = TokenStats::from_transcript(&entries, true);
        assert!(live.live);
        assert_eq!(live.label(), "10.0 tok/s");
        assert_eq!(live.rate_description(), "Live average TPS");
        assert_eq!(rows(live)[1].1, "500");
    }

    #[test]
    fn live_rate_holds_between_reports_and_uses_new_snapshot_duration() {
        let mut current = assistant(Some(TokenUsage {
            output_tokens: Some(100),
            ..Default::default()
        }));
        current.status = Some(MessageStatus::Streaming);
        current.duration_ms = Some(10_000);
        let mut entries = [current];
        let first = TokenStats::from_transcript(&entries, true);
        assert_eq!(first.label(), "10.0 tok/s");

        entries[0].parts.push(MessagePart::Reasoning {
            id: "reasoning".into(),
            text: "Still working".into(),
        });
        assert_eq!(TokenStats::from_transcript(&entries, true), first);
        entries[0].parts.push(MessagePart::Text {
            id: "text".into(),
            text: "More content without a new token report".into(),
        });
        assert_eq!(TokenStats::from_transcript(&entries, true), first);
        let reopened = entries.clone();
        assert_eq!(TokenStats::from_transcript(&reopened, true), first);

        entries[0].token_usage.as_mut().unwrap().output_tokens = Some(200);
        entries[0].duration_ms = Some(25_000);
        let updated = TokenStats::from_transcript(&entries, true);
        assert_eq!(updated.label(), "8.0 tok/s");
        assert_eq!(updated.rate_description(), "Live average TPS");
        assert_eq!(rows(updated)[1].1, "200");
        assert_eq!(TokenStats::from_transcript(&entries, true), updated);
    }

    #[test]
    fn completed_rate_uses_full_turn_duration_including_waiting() {
        let mut current = assistant(Some(TokenUsage {
            output_tokens: Some(100),
            ..Default::default()
        }));
        current.status = Some(MessageStatus::Streaming);
        current.duration_ms = Some(10_000);
        let mut entries = [current];
        assert_eq!(
            TokenStats::from_transcript(&entries, true).label(),
            "10.0 tok/s"
        );
        entries[0].status = Some(MessageStatus::Complete);
        entries[0].duration_ms = Some(20_000);
        let completed = TokenStats::from_transcript(&entries, false);
        assert!(!completed.live);
        assert_eq!(completed.label(), "5.0 tok/s");
        assert_eq!(completed.rate_description(), "Average TPS (last turn)");
        assert_eq!(rows(completed)[1].1, "100");
    }

    #[test]
    fn a_first_turn_without_counts_has_no_loading_label_or_invented_rate() {
        let mut current = assistant(None);
        current.status = Some(MessageStatus::Streaming);
        assert_eq!(
            TokenStats::from_transcript(&[current], true).label(),
            "- tok/s"
        );
    }

    #[test]
    fn latest_missing_usage_preserves_totals_but_does_not_reuse_older_tps() {
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
            let stats = TokenStats::from_transcript(&entries, false);
            assert_eq!(stats.label(), "- tok/s");
            assert_eq!(rows(stats)[1].1, "400");
            assert_eq!(rows(stats)[7].1, "Not reported");
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
            TokenStats::from_transcript(&entries, false).label(),
            "20.0 tok/s"
        );
    }

    #[test]
    fn missing_counts_stay_distinct_from_reported_zero() {
        let stats = TokenStats::from_transcript(
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

    #[test]
    fn thread_totals_sum_turns_without_changing_last_turn_tps() {
        let entries = [
            assistant(Some(TokenUsage {
                input_tokens: Some(1000),
                output_tokens: Some(400),
                cached_input_tokens: Some(500),
                cache_write_input_tokens: Some(100),
                reasoning_output_tokens: Some(100),
                cost_usd: Some(0.1),
                ..Default::default()
            })),
            assistant(Some(TokenUsage {
                input_tokens: Some(3000),
                output_tokens: Some(600),
                cached_input_tokens: Some(2000),
                cache_write_input_tokens: Some(200),
                reasoning_output_tokens: Some(150),
                cost_usd: Some(0.2),
                ..Default::default()
            })),
        ];
        let stats = TokenStats::from_transcript(&entries, false);
        let values = rows(stats);
        assert_eq!(values[0].1, "4,000");
        assert_eq!(values[1].1, "1,000");
        assert_eq!(values[2].1, "5,000");
        assert_eq!(values[3].1, "2,500");
        assert_eq!(values[4].1, "62.5%");
        assert_eq!(values[5].1, "300");
        assert_eq!(values[6].1, "250");
        assert_eq!(values[7].1, "30.0 tok/s");
        assert_eq!(values[8].1, "$0.3000");
        assert_eq!(stats.label(), "30.0 tok/s");
        assert_eq!(stats, TokenStats::from_transcript(&entries, false));
    }

    #[test]
    fn partially_reported_turns_preserve_known_counts_without_inventing_totals() {
        let entries = [
            assistant(Some(TokenUsage {
                input_tokens: Some(1000),
                cache_write_input_tokens: Some(100),
                cost_usd: Some(0.1),
                ..Default::default()
            })),
            assistant(Some(TokenUsage {
                output_tokens: Some(600),
                cost_usd: Some(f64::NAN),
                ..Default::default()
            })),
        ];
        let values = rows(TokenStats::from_transcript(&entries, false));
        assert_eq!(values[0].1, "1,000");
        assert_eq!(values[1].1, "600");
        assert_eq!(values[2].1, "Not reported");
        assert_eq!(values[5].1, "100");
        assert_eq!(values[8].1, "$0.1000");
    }

    #[test]
    fn missing_cache_counts_are_not_treated_as_zero_for_the_hit_rate() {
        let entries = [
            assistant(Some(TokenUsage {
                input_tokens: Some(1000),
                cached_input_tokens: Some(500),
                ..Default::default()
            })),
            assistant(Some(TokenUsage {
                input_tokens: Some(1000),
                ..Default::default()
            })),
        ];
        let values = rows(TokenStats::from_transcript(&entries, false));
        assert_eq!(values[0].1, "2,000");
        assert_eq!(values[3].1, "500");
        assert_eq!(values[4].1, "Not reported");
    }

    #[test]
    fn non_assistant_entries_and_other_threads_do_not_contribute() {
        let mut user = assistant(Some(TokenUsage {
            output_tokens: Some(1000),
            ..Default::default()
        }));
        user.role = MessageRole::User;
        let entries = [
            user,
            assistant(Some(TokenUsage {
                output_tokens: Some(400),
                ..Default::default()
            })),
        ];
        assert_eq!(
            rows(TokenStats::from_transcript(&entries, false))[1].1,
            "400"
        );
        assert!(
            rows(TokenStats::from_transcript(&[], false))
                .iter()
                .all(|(_, value)| value == "Not reported")
        );
        assert_eq!(
            rows(TokenStats::from_transcript(&entries, false))[1].1,
            "400"
        );
    }

    #[test]
    fn joined_continuations_contribute_the_latest_turn_snapshot_once() {
        let root = assistant(Some(TokenUsage {
            output_tokens: Some(100),
            ..Default::default()
        }));
        let mut tail = assistant(Some(TokenUsage {
            output_tokens: Some(400),
            ..Default::default()
        }));
        tail.id = "continuation".into();
        tail.continuation_of = Some(root.id.clone());
        let entries = zeron_doc::join_continuation_entries(vec![root, tail]);
        assert_eq!(
            rows(TokenStats::from_transcript(&entries, false))[1].1,
            "400"
        );
    }
}

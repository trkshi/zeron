//! Pure token statistics; rendering never retains another transcript copy.
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, TranscriptFrame};
use zeron_proto::{GenerationUsage, TokenUsage};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TokenStats {
    pub(super) thread_usage: TokenUsage,
    pub(super) cache_hit_rate: Option<f64>,
    pub(super) turn_usage: Option<TokenUsage>,
    pub(super) duration_ms: Option<i64>,
    pub(super) live: bool,
    pub(super) previous: bool,
    pub(super) generation: Option<GenerationUsage>,
    pub(super) generation_previous: bool,
}

impl TokenStats {
    pub fn from_transcript(entries: &[SessionMessageEntry], working: bool) -> Self {
        let mut stats = Self {
            thread_usage: thread_usage(entries),
            ..Default::default()
        };
        // A cache-hit rate needs complete reported input/cache counts.
        if entries
            .iter()
            .filter(|entry| entry.role == MessageRole::Assistant)
            .filter_map(|entry| entry.token_usage.as_deref())
            .all(|usage| {
                usage
                    .input_tokens
                    .zip(usage.cached_input_tokens)
                    .is_some_and(|(input, cached)| cached <= input)
            })
        {
            stats.cache_hit_rate = stats.thread_usage.cache_hit_rate();
        }
        let latest = entries.iter().rev().find(|entry| is_turn(entry));
        let live = latest.filter(|entry| {
            working
                && entry.status == Some(MessageStatus::Streaming)
                && entry.duration_ms.is_some_and(|ms| ms > 0)
                && entry
                    .token_usage
                    .as_deref()
                    .and_then(|usage| usage.output_tokens)
                    .is_some_and(|output| output > 0)
        });
        stats.live = live.is_some();
        // Keep the last completed rate until this turn reports real output.
        let entry = live.or_else(|| {
            if working {
                entries
                    .iter()
                    .rev()
                    .find(|entry| is_turn(entry) && entry.status != Some(MessageStatus::Streaming))
            } else {
                latest
            }
        });
        if let Some(entry) = entry {
            // Keep counts paired with their reported duration. Advancing only
            // elapsed time would decay TPS without fresh token telemetry.
            stats.turn_usage = entry.token_usage.as_deref().copied();
            stats.duration_ms = entry.duration_ms;
        }
        stats.previous = working && !stats.live;
        stats.generation = stats.turn_usage.and_then(|usage| usage.generation);
        stats.generation_previous = stats.previous;
        if working && stats.generation.and_then(GenerationUsage::tps).is_none() {
            stats.generation = entries
                .iter()
                .rev()
                .find(|entry| is_turn(entry) && entry.status != Some(MessageStatus::Streaming))
                .and_then(|entry| entry.token_usage.as_deref()?.generation)
                .filter(|generation| generation.tps().is_some());
            stats.generation_previous = true;
        }
        stats
    }

    pub fn rate_description(self) -> &'static str {
        if self.generation.and_then(GenerationUsage::tps).is_some() {
            self.generation_description()
        } else {
            self.average_description()
        }
    }

    pub(super) fn generation_description(self) -> &'static str {
        match (
            self.generation
                .is_some_and(|generation| generation.estimated),
            self.generation_previous,
        ) {
            (true, true) => "Estimated generation TPS (previous turn)",
            (true, false) => "Estimated generation TPS (latest request)",
            (false, true) => "Generation TPS (previous turn)",
            (false, false) => "Generation TPS (latest request)",
        }
    }

    pub(super) fn average_description(self) -> &'static str {
        if self.live {
            "Live average TPS"
        } else if self.previous {
            "Average TPS (previous turn)"
        } else {
            "Average TPS (last turn)"
        }
    }

    pub fn label(self) -> String {
        self.generation_label()
            .unwrap_or_else(|| self.average_label())
    }

    pub(super) fn generation_label(self) -> Option<String> {
        let generation = self.generation?;
        let tps = generation.tps()?;
        let prefix = if generation.estimated { "~" } else { "" };
        Some(format!("{prefix}{tps:.1} tok/s"))
    }

    fn average_label(self) -> String {
        self.turn_usage
            .and_then(|usage| usage.average_tps(self.duration_ms))
            .map(|tps| format!("{tps:.1} tok/s"))
            .unwrap_or_else(|| "- tok/s".into())
    }
}

// Late asynchronous questions are not new measured turns.
fn is_turn(entry: &SessionMessageEntry) -> bool {
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
}

fn thread_usage(entries: &[SessionMessageEntry]) -> TokenUsage {
    let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    // The UI transcript has already joined continuations. Recompute from its
    // turn snapshots so rerenders, reconnects, and thread switches cannot add twice.
    entries
        .iter()
        .filter(|entry| entry.role == MessageRole::Assistant)
        .filter_map(|entry| entry.token_usage.as_deref())
        .fold(TokenUsage::default(), |total, usage| {
            let reported_cost = usage
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0);
            let cost_usd = match (total.cost_usd, reported_cost) {
                (Some(a), Some(b)) => Some(a + b).filter(|cost| cost.is_finite()),
                (a, b) => a.or(b),
            };
            TokenUsage {
                input_tokens: add(total.input_tokens, usage.input_tokens),
                output_tokens: add(total.output_tokens, usage.output_tokens),
                total_tokens: add(total.total_tokens, usage.total()),
                cached_input_tokens: add(total.cached_input_tokens, usage.cached_input_tokens),
                cache_write_input_tokens: add(
                    total.cache_write_input_tokens,
                    usage.cache_write_input_tokens,
                ),
                reasoning_output_tokens: add(
                    total.reasoning_output_tokens,
                    usage.reasoning_output_tokens,
                ),
                cost_usd,
                generation: None,
            }
        })
}

/// One selected transcript's scalar statistics, invalidated by non-text
/// transcript updates and turn-state changes. No per-chat history is retained.
#[derive(Default)]
pub(crate) struct TokenStatsCache {
    key: Option<(Option<String>, u64, bool)>,
    stats: TokenStats,
    #[cfg(test)]
    recomputations: usize,
}

pub(crate) fn invalidates_stats(frame: &TranscriptFrame) -> bool {
    match frame {
        TranscriptFrame::Reset { .. } => true,
        TranscriptFrame::Delta { upsert, remove, .. } => !upsert.is_empty() || !remove.is_empty(),
    }
}

impl TokenStatsCache {
    pub fn get(
        &mut self,
        chat_id: Option<&str>,
        revision: u64,
        working: bool,
        entries: &[SessionMessageEntry],
    ) -> TokenStats {
        if self.key.as_ref().is_none_or(|(chat, rev, active)| {
            chat.as_deref() != chat_id || *rev != revision || *active != working
        }) {
            self.stats = TokenStats::from_transcript(entries, working);
            self.key = Some((chat_id.map(str::to_owned), revision, working));
            #[cfg(test)]
            {
                self.recomputations += 1;
            }
        }
        self.stats
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn assistant(output: u64) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "turn".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "text".into(),
                text: "reply".into(),
            }],
            created_at: 1,
            device_id: "host".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: Some(20_000),
            token_usage: Some(Box::new(TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(output),
                cached_input_tokens: Some(50),
                ..Default::default()
            })),
        }
    }

    #[test]
    fn repeated_renders_and_text_growth_reuse_one_calculation() {
        let mut cache = TokenStatsCache::default();
        let mut entries: Vec<_> = (0..10_000).map(|_| assistant(20)).collect();
        let expected = TokenStats::from_transcript(&entries, false);
        let uncached_started = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(TokenStats::from_transcript(
                std::hint::black_box(&entries),
                false,
            ));
        }
        let uncached = uncached_started.elapsed();
        let cached_started = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(cache.get(Some("chat"), 1, false, &entries), expected);
        }
        let cached = cached_started.elapsed();
        println!("10,000 turns / 1,000 footer reads: uncached={uncached:?}, cached={cached:?}");
        if let MessagePart::Text { text, .. } = &mut entries.last_mut().unwrap().parts[0] {
            text.push_str(" more text");
        }
        assert_eq!(cache.get(Some("chat"), 1, false, &entries), expected);
        assert_eq!(cache.recomputations, 1);
    }

    #[test]
    fn only_resets_upserts_and_removals_invalidate_stats() {
        let empty = TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![],
            remove: vec![],
            count: 1,
        };
        assert!(!invalidates_stats(&empty));
        let append = TranscriptFrame::Delta {
            append: vec![zeron_doc::TextAppend {
                entry: "turn".into(),
                part: "text".into(),
                text: "!".into(),
                len: 6,
            }],
            upsert: vec![],
            remove: vec![],
            count: 1,
        };
        assert!(!invalidates_stats(&append));
        let upsert = TranscriptFrame::Delta {
            upsert: vec![zeron_doc::TranscriptUpsert {
                after: None,
                entry: assistant(200),
            }],
            append: vec![],
            remove: vec![],
            count: 1,
        };
        assert!(invalidates_stats(&upsert));
        let remove = TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![],
            remove: vec!["turn".into()],
            count: 0,
        };
        assert!(invalidates_stats(&remove));
        assert!(invalidates_stats(&TranscriptFrame::reset(&[])));
        assert!(invalidates_stats(&TranscriptFrame::reset(&[assistant(
            200
        )])));
    }

    #[test]
    fn usage_duration_status_and_generation_updates_invalidate_cached_stats() {
        let mut cache = TokenStatsCache::default();
        let mut entries = [assistant(100)];
        let initial = cache.get(Some("chat"), 1, true, &entries);
        assert!(initial.previous);
        entries[0].status = Some(MessageStatus::Streaming);
        entries[0].token_usage.as_mut().unwrap().output_tokens = Some(200);
        entries[0].duration_ms = Some(10_000);
        let live = cache.get(Some("chat"), 2, true, &entries);
        assert_eq!(live.label(), "20.0 tok/s");
        assert_eq!(live, TokenStats::from_transcript(&entries, true));

        entries[0].token_usage.as_mut().unwrap().generation = Some(GenerationUsage {
            output_tokens: 200,
            reasoning_output_tokens: Some(40),
            elapsed_ms: 2000,
            ttft_ms: 1000,
            estimated: true,
        });
        let measured = cache.get(Some("chat"), 3, true, &entries);
        assert_eq!(measured.label(), "~160.0 tok/s");
        assert_eq!(measured, TokenStats::from_transcript(&entries, true));

        entries[0].status = Some(MessageStatus::Complete);
        entries[0].duration_ms = Some(30_000);
        let completed = cache.get(Some("chat"), 4, false, &entries);
        assert_eq!(completed, TokenStats::from_transcript(&entries, false));
        assert_eq!(cache.recomputations, 4);
    }

    #[test]
    fn switching_threads_with_the_same_revision_cannot_reuse_another_threads_totals() {
        let mut cache = TokenStatsCache::default();
        let first = [assistant(100)];
        let second = [assistant(300)];
        assert_eq!(
            cache
                .get(Some("first"), 1, false, &first)
                .thread_usage
                .output_tokens,
            Some(100)
        );
        assert_eq!(
            cache
                .get(Some("second"), 1, false, &second)
                .thread_usage
                .output_tokens,
            Some(300)
        );
        assert_eq!(
            cache
                .get(Some("first"), 1, false, &first)
                .thread_usage
                .output_tokens,
            Some(100)
        );
        assert_eq!(cache.get(None, 1, false, &[]), TokenStats::default());
        assert_eq!(cache.recomputations, 4);
    }

    #[test]
    fn reconnect_removal_and_joined_continuations_replace_instead_of_adding_totals() {
        let mut cache = TokenStatsCache::default();
        let root = assistant(100);
        let mut tail = assistant(300);
        tail.id = "continuation".into();
        tail.continuation_of = Some(root.id.clone());
        let joined = zeron_doc::join_continuation_entries(vec![root, tail]);
        let initial = cache.get(Some("chat"), 1, false, &joined);
        assert_eq!(initial.thread_usage.output_tokens, Some(300));
        assert_eq!(cache.get(Some("chat"), 2, false, &joined), initial);
        assert_eq!(
            cache.get(Some("chat"), 3, false, &[]),
            TokenStats::default()
        );
        assert_eq!(cache.get(Some("chat"), 4, false, &joined), initial);
        assert_eq!(cache.recomputations, 4);
    }

    #[test]
    fn turn_state_and_late_async_questions_are_handled_without_stale_rates() {
        let mut cache = TokenStatsCache::default();
        let mut entries = vec![assistant(100)];
        let completed = cache.get(Some("chat"), 1, false, &entries);
        let working = cache.get(Some("chat"), 1, true, &entries);
        assert!(!completed.previous);
        assert!(working.previous);
        assert_eq!(working, TokenStats::from_transcript(&entries, true));

        let mut question = assistant(0);
        question.id = "question".into();
        question.token_usage = None;
        question.duration_ms = None;
        question.parts = vec![MessagePart::Input {
            id: "part".into(),
            request_id: "question".into(),
            questions: vec![],
            resolved: false,
            asynchronous: true,
        }];
        entries.push(question);
        assert_eq!(cache.get(Some("chat"), 2, false, &entries), completed);
        assert_eq!(cache.recomputations, 3);
    }
}

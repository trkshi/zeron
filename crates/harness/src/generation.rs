//! Client-observed streaming windows paired with provider-reported counts.
use tokio::time::Instant;
use zeron_proto::{GenerationUsage, TokenUsage};

#[derive(Default)]
pub(crate) struct GenerationTimer {
    first: Option<Instant>,
    last: Option<Instant>,
    closed: bool,
    unavailable: bool,
}

impl GenerationTimer {
    pub fn output(&mut self) {
        if self.closed || self.unavailable {
            return;
        }
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    pub fn finish(&mut self) {
        self.closed = true;
    }

    pub fn invalidate(&mut self) {
        self.unavailable = true;
    }

    pub fn generation(&self, usage: TokenUsage) -> Option<GenerationUsage> {
        if !self.closed || self.unavailable {
            return None;
        }
        // Stop at the last generated output, not the later tool/result frame.
        // A single buffered chunk provides no measurable streaming window.
        let duration = self.last?.checked_duration_since(self.first?)?;
        let generation = GenerationUsage {
            output_tokens: usage.output_tokens?,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            elapsed_ms: u64::try_from(duration.as_millis()).ok()?,
            ttft_ms: 0,
            estimated: true,
        };
        generation.tps().map(|_| generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn timing_excludes_initial_wait_and_trailing_tool_work() {
        let mut timer = GenerationTimer::default();
        let usage = TokenUsage {
            output_tokens: Some(120),
            reasoning_output_tokens: Some(20),
            ..Default::default()
        };
        tokio::time::advance(Duration::from_secs(12)).await;
        timer.output();
        tokio::time::advance(Duration::from_secs(2)).await;
        timer.output();
        assert_eq!(timer.generation(usage), None);
        tokio::time::advance(Duration::from_secs(30)).await;
        timer.finish();
        let generation = timer.generation(usage).unwrap();
        assert!(generation.estimated);
        assert_eq!(generation.elapsed_ms, 2000);
        assert_eq!(generation.tps(), Some(50.0));
        tokio::time::advance(Duration::from_secs(30)).await;
        timer.output();
        timer.finish();
        assert_eq!(timer.generation(usage), Some(generation));
    }

    #[tokio::test(start_paused = true)]
    async fn missing_counts_single_chunks_and_stream_gaps_do_not_invent_a_rate() {
        let usage = TokenUsage {
            output_tokens: Some(100),
            ..Default::default()
        };
        let mut timer = GenerationTimer::default();
        timer.output();
        timer.finish();
        assert_eq!(timer.generation(usage), None);

        let mut timer = GenerationTimer::default();
        timer.output();
        tokio::time::advance(Duration::from_secs(2)).await;
        timer.output();
        timer.finish();
        assert_eq!(timer.generation(TokenUsage::default()), None);
        assert_eq!(timer.generation(usage).unwrap().tps(), Some(50.0));
        // Unknown reasoning stays unknown; it is not guessed from text.
        assert_eq!(
            timer.generation(usage).unwrap().reasoning_output_tokens,
            None
        );
        timer.invalidate();
        assert_eq!(timer.generation(usage), None);
    }
}

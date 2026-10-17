//! Generation TPS: provider-reported output over the time each model request
//! spent streaming. Waiting for the first token and tool runs between
//! requests fall outside every window. Short replies can arrive as a single
//! burst, so a request needs a minimum window and chunk count to be measured;
//! one that does not qualify is left out of both the tokens and the time.
use std::time::{Duration, Instant};

use zeron_proto::GenerationUsage;

const MIN_WINDOW: Duration = Duration::from_millis(300);
const MIN_CHUNKS: u32 = 3;

/// One request's streaming window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamWindow {
    start: Instant,
    chunks: u32,
}

impl StreamWindow {
    pub fn open(now: Instant) -> Self {
        Self {
            start: now,
            chunks: 0,
        }
    }

    pub fn chunk(&mut self) {
        self.chunks = self.chunks.saturating_add(1);
    }

    /// The window closed at `end`, when it streamed long enough to measure.
    pub fn measured(self, end: Instant) -> Option<Duration> {
        let elapsed = end.checked_duration_since(self.start)?;
        qualifies(self.chunks, elapsed).then_some(elapsed)
    }
}

/// Whether a window streamed long enough, in enough chunks, to be measured.
pub(crate) fn qualifies(chunks: u32, elapsed: Duration) -> bool {
    chunks >= MIN_CHUNKS && elapsed >= MIN_WINDOW
}

/// Add one measured request to the turn's generation reading.
pub(crate) fn accumulate(
    total: Option<GenerationUsage>,
    output_tokens: u64,
    elapsed: Duration,
) -> Option<GenerationUsage> {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).ok()?;
    if output_tokens == 0 || elapsed_ms == 0 {
        return total;
    }
    let total = total.unwrap_or_default();
    Some(GenerationUsage {
        output_tokens: total.output_tokens.saturating_add(output_tokens),
        reasoning_output_tokens: None,
        elapsed_ms: total.elapsed_ms.saturating_add(elapsed_ms),
        ttft_ms: 0,
        estimated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_and_short_windows_are_not_measured() {
        let start = Instant::now();
        let mut window = StreamWindow::open(start);
        window.chunk();
        assert_eq!(window.measured(start + Duration::from_secs(5)), None);
        window.chunk();
        window.chunk();
        assert_eq!(window.measured(start + Duration::from_millis(17)), None);
        assert_eq!(
            window.measured(start + Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn requests_add_tokens_and_time() {
        let first = accumulate(None, 905, Duration::from_millis(9_630)).unwrap();
        let both = accumulate(Some(first), 95, Duration::from_millis(370)).unwrap();
        assert_eq!(both.output_tokens, 1_000);
        assert_eq!(both.elapsed_ms, 10_000);
        assert_eq!(both.tps(), Some(100.0));
        assert_eq!(accumulate(Some(both), 0, Duration::from_secs(1)), Some(both));
        assert_eq!(accumulate(None, 10, Duration::ZERO), None);
    }
}

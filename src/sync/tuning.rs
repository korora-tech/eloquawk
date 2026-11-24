use super::*;

/// The poll interval, page size and backoff schedule. Tests shrink the durations.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tuning {
    pub(super) refresh: Duration,
    pub(super) page_size: u32,
    pub(super) backoff_base: Duration,
    pub(super) backoff_cap: Duration,
    pub(super) jitter_max: Duration,
}

impl Tuning {
    pub(super) fn new(refresh_secs: u64, page_size: u32) -> Self {
        Self {
            refresh: Duration::from_secs(refresh_secs.max(1)),
            page_size: page_size.max(1),
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(300),
            jitter_max: Duration::from_millis(1000),
        }
    }

    /// A random delay below `jitter_max`, so accounts and clients do not retry in step.
    pub(super) fn jitter(&self) -> Duration {
        let max = u64::try_from(self.jitter_max.as_millis()).unwrap_or(u64::MAX);
        if max == 0 {
            return Duration::ZERO;
        }
        let mut buf = [0u8; 8];
        if getrandom::fill(&mut buf).is_err() {
            return Duration::ZERO;
        }
        Duration::from_millis(u64::from_le_bytes(buf) % max)
    }
}

/// An account's retry schedule after retryable failures (SPEC 7.6). Pure: every method takes
/// `now`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Backoff {
    pub(super) attempts: u32,
    pub(super) retry_at: Option<Instant>,
}

impl Backoff {
    /// Whether the account must still wait.
    pub(super) fn waiting(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|t| t > now)
    }

    /// A rate limit, network failure, 5xx or unreadable answer: wait
    /// `max(hint, min(base·2^attempts, cap)) + jitter`. While the account is already waiting
    /// only interactive jobs run, and their failures (clicks while offline) must not hurry
    /// the schedule towards the cap: they only lengthen the wait to a longer hint.
    pub(super) fn failed(
        &mut self,
        now: Instant,
        hint: Option<Duration>,
        jitter: Duration,
        t: &Tuning,
    ) {
        if self.waiting(now) {
            if let Some(hint) = hint {
                self.retry_at = self.retry_at.max(Some(now + hint));
            }
            return;
        }
        let factor = 1u32.checked_shl(self.attempts).unwrap_or(u32::MAX);
        let exp = t.backoff_base.saturating_mul(factor).min(t.backoff_cap);
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at = Some(now + hint.unwrap_or_default().max(exp) + jitter);
    }

    /// A daily or sending limit: nothing automatic before the hint or 15 minutes, whichever
    /// is later.
    pub(super) fn quota(&mut self, now: Instant, hint: Option<Duration>) {
        self.attempts = self.attempts.saturating_add(1);
        let until = now + hint.unwrap_or_default().max(QUOTA_WAIT);
        self.retry_at = self.retry_at.max(Some(until));
    }

    /// A success, or a manual Refresh: start afresh.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

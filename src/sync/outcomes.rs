use super::*;

impl Worker {
    // ----- outcomes -------------------------------------------------------------------------

    /// A call for `email` worked: its backoff starts afresh, and a network problem, or the
    /// account's problem when it came from the failure that backed it off (a rate limit, a
    /// 5xx, a quota), is over.
    pub(super) fn succeeded(&mut self, email: &str) {
        let mut resolved = false;
        if let Some(account) = self.accounts.get_mut(email)
            && account.backoff != Backoff::default()
        {
            account.backoff.reset();
            resolved |= self.problems.remove(email).is_some();
        }
        resolved |= self.problems.remove("network").is_some();
        if resolved {
            self.report();
        }
    }

    /// Handles a failed call for `email` (SPEC 7.6) and says whether it is worth retrying
    /// after the backoff: rate limits, network failures, 5xx and unreadable answers back off
    /// the account; quota problems wait at least 15 minutes; a refused or missing item is a
    /// problem but the sync carries on; an oversized or missing item is reported by the
    /// caller.
    pub(super) fn failed(&mut self, email: &str, error: &CallError) -> bool {
        self.failed_as(email, error, true)
    }

    /// [`Self::failed`], with `report` false for a background AI prefetch (SPEC 16.11): the
    /// account still backs off, but nothing reaches the problems banner.
    pub(super) fn failed_as(&mut self, email: &str, error: &CallError, report: bool) -> bool {
        let now = Instant::now();
        let jitter = self.tuning.jitter();
        let tuning = self.tuning;
        let mut backoff = |hint: Option<Duration>, quota: bool| {
            if let Some(account) = self.accounts.get_mut(email) {
                if quota {
                    account.backoff.quota(now, hint);
                } else {
                    account.backoff.failed(now, hint, jitter, &tuning);
                }
            }
        };
        let (key, message, retry) = match error {
            // `access_token` reported these itself.
            CallError::Auth | CallError::Gone => return false,
            CallError::Network => {
                backoff(None, false);
                return true;
            }
            CallError::Api(ApiError::Network(_)) => {
                backoff(None, false);
                ("network".to_string(), NETWORK_PROBLEM.to_string(), true)
            }
            // `classify` already turns these into `QuotaExhausted`; this guards any other
            // source of a rate limit too long to be a burst (SPEC 7.6).
            CallError::Api(
                e @ ApiError::RateLimited {
                    retry_after: Some(hint),
                },
            ) if *hint > QUOTA_WAIT => {
                backoff(Some(*hint), true);
                let quota = ApiError::QuotaExhausted {
                    message: e.to_string(),
                    retry_after: Some(*hint),
                };
                (email.to_string(), format!("{email}: {quota}"), true)
            }
            CallError::Api(e @ ApiError::RateLimited { retry_after }) => {
                backoff(*retry_after, false);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(e @ (ApiError::Decode(_) | ApiError::Http { status: 500.., .. })) => {
                backoff(None, false);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(e @ ApiError::QuotaExhausted { retry_after, .. }) => {
                backoff(*retry_after, true);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(ApiError::NotFound | ApiError::TooLarge) => return false,
            CallError::Api(e) if e.missing_scope() => {
                (email.to_string(), missing_scope_message(email), false)
            }
            CallError::Api(e) => (email.to_string(), format!("{email}: {e}"), false),
        };
        if report {
            self.problems.insert(key, message);
            self.report();
        }
        retry
    }

    /// Parks `job` on its account until the backoff passes.
    pub(super) fn defer(&mut self, job: Job) {
        if let Some(account) = self.accounts.get_mut(job.account()) {
            account.defer(job);
        }
    }

    // ----- jobs -----------------------------------------------------------------------------
}

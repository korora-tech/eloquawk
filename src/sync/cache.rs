use super::*;

impl Worker {
    // ----- summary cache (SPEC 7.7) ---------------------------------------------------------

    /// Opens the cache once the keyring is readable. Its key is made on first use.
    pub(super) fn open_cache(&mut self) -> Option<&MailCache> {
        if self.cache.is_none() && !self.cache_failed {
            let dir = self.cache_dir.clone()?;
            match self.secrets.cache_key() {
                Ok(key) => self.cache = Some(MailCache::new(dir, &key)),
                Err(e) => {
                    eprintln!("eloquawk: keeping no summary cache: {e}");
                    self.cache_failed = true;
                }
            }
        }
        self.cache.as_ref()
    }

    /// Shows what the cache holds for `email` straight away: its labels and the cached
    /// pages. The listings that follow reuse the summaries whose history id still matches,
    /// so only changed threads are fetched.
    pub(super) fn restore(&mut self, email: &str) {
        let Some(snapshot) = self.open_cache().and_then(|c| c.load(email)) else {
            return;
        };
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        if !account.restore(snapshot) {
            return;
        }
        let labels: Vec<Label> = account.labels.values().cloned().collect();
        let mut pages = Vec::new();
        for (kind, ids) in &account.listed {
            let threads: Vec<ThreadSummary> = ids
                .iter()
                .filter_map(|id| account.known.get(id).map(|k| k.summary.clone()))
                .collect();
            let ids = threads.iter().map(|s| s.key.id.clone()).collect();
            let view = ViewKey {
                account: email.to_string(),
                kind: kind.clone(),
            };
            pages.push((view, ids, threads));
        }
        if !labels.is_empty() {
            self.emit(SyncEvent::Labels {
                account: email.to_string(),
                labels,
            });
        }
        for (view, ids, threads) in pages {
            // `Refresh` fills an empty list without ending its loading state, so the real
            // listing still replaces it; `has_more` waits for that listing too.
            self.emit(SyncEvent::ThreadPage {
                view,
                ids,
                threads,
                mode: PageMode::Refresh,
                has_more: false,
            });
        }
    }

    /// Writes every changed account's snapshot.
    pub(super) fn persist(&mut self) {
        if !self.accounts.values().any(|a| a.dirty) || self.open_cache().is_none() {
            return;
        }
        let Some(cache) = &self.cache else {
            return;
        };
        for (email, account) in self.accounts.iter_mut().filter(|(_, a)| a.dirty) {
            account.dirty = false;
            if let Err(e) = cache.save(email, &account.snapshot()) {
                eprintln!("eloquawk: could not write the summary cache: {}", e.kind());
            }
        }
    }
}

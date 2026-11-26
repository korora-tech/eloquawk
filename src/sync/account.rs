use super::*;

/// Whether the sidebar shows a label, so its count is worth fetching.
pub(super) fn shown(l: &Label) -> bool {
    match l.kind {
        LabelKind::System => SHOWN_SYSTEM_LABELS.contains(&l.id.as_str()),
        LabelKind::User => !l.hidden,
    }
}

pub(super) struct AccountState {
    pub(super) refresh_token: String,
    pub(super) access: Option<Access>,
    /// The history cursor, recorded by `profile` before listing.
    pub(super) history_id: Option<u64>,
    pub(super) synced_once: bool,
    pub(super) known: HashMap<String, Known>,
    /// The next page token per view; `None` once the last page is in.
    pub(super) pages: HashMap<ViewKind, Option<String>>,
    /// Listed ids still without a summary (skeleton rows), with their listed history ids.
    pub(super) missing: BTreeMap<String, u64>,
    /// Ids whose metadata Gmail refused without a retry (403, too large), with the history
    /// id they were refused at. Neither the poll nor a listing fetches them again until a
    /// listing shows a new history id.
    pub(super) refused: HashMap<String, u64>,
    /// Thread ids a history round found changed whose refetch has not succeeded yet. They
    /// survive failures, so moving the cursor can never lose a change.
    pub(super) pending_changed: BTreeSet<String>,
    /// Candidate new-mail thread ids not reported yet.
    pub(super) pending_fresh: BTreeSet<String>,
    /// Labels whose counts the round in progress moved.
    pub(super) round_labels: BTreeSet<String>,
    /// Background jobs parked while in backoff.
    pub(super) deferred: Vec<Job>,
    pub(super) backoff: Backoff,
    /// The last labels and counts, so a full list keeps the counts while new ones load.
    pub(super) labels: BTreeMap<String, Label>,
    /// Page 1 of each label view listed, as the summary cache keeps it (SPEC 7.7).
    pub(super) listed: HashMap<ViewKind, Vec<String>>,
    /// Something the summary cache holds changed since it was last written.
    pub(super) dirty: bool,
    /// This session resumed from a saved cursor: the first round finds what changed while the
    /// app was closed, which is not new mail, and then refreshes the label list.
    pub(super) resumed: bool,
    /// Dismissed nudges, as sealed on disk (`dismissals.rs`).
    pub(super) dismissed: Dismissed,
}

impl AccountState {
    pub(super) fn new(refresh_token: String, access: Option<Access>) -> Self {
        Self {
            refresh_token,
            access,
            history_id: None,
            synced_once: false,
            known: HashMap::new(),
            pages: HashMap::new(),
            missing: BTreeMap::new(),
            refused: HashMap::new(),
            pending_changed: BTreeSet::new(),
            pending_fresh: BTreeSet::new(),
            round_labels: BTreeSet::new(),
            deferred: Vec::new(),
            backoff: Backoff::default(),
            labels: BTreeMap::new(),
            listed: HashMap::new(),
            dirty: false,
            resumed: false,
            dismissed: Dismissed::new(),
        }
    }

    /// What the summary cache keeps: the labels, page 1 of each listed label view and the
    /// summaries those pages show.
    pub(super) fn snapshot(&self) -> Snapshot {
        let mut seen = HashSet::new();
        let threads = self
            .listed
            .values()
            .flatten()
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| self.known.get(id))
            .map(|k| CachedThread {
                star_target: k.star_target.clone(),
                summary: k.summary.clone(),
            })
            .collect();
        Snapshot {
            // Only a state that every change up to the cursor has been applied to. Mid-round
            // (changes still pending) the saved cursor would skip them, so none is saved and
            // the next launch lists afresh.
            history_id: self
                .history_id
                .filter(|_| self.synced_once && self.pending_changed.is_empty()),
            labels: self.labels.values().cloned().collect(),
            views: self
                .listed
                .iter()
                .map(|(k, ids)| (k.clone(), ids.clone()))
                .collect(),
            threads,
        }
    }

    /// Takes a cache snapshot in, unless this session already has newer data.
    pub(super) fn restore(&mut self, snapshot: Snapshot) -> bool {
        if !self.known.is_empty() || !self.labels.is_empty() {
            return false;
        }
        // A summary cached before the newest sender was kept cannot be judged for nudges; it
        // is left out so the next listing fetches it again.
        for t in snapshot
            .threads
            .into_iter()
            .filter(|t| t.summary.last.is_some())
        {
            self.known.insert(
                t.summary.key.id.clone(),
                Known {
                    history_id: t.summary.history_id,
                    star_target: t.star_target,
                    summary: t.summary,
                },
            );
        }
        self.labels = snapshot
            .labels
            .into_iter()
            .map(|l| (l.id.clone(), l))
            .collect();
        self.listed = snapshot.views.into_iter().collect();
        // Resume from the saved cursor when there is one: the History API then yields just
        // what changed since, in place of listing everything again. Gmail answers 404 for a
        // cursor it no longer has, which resets the account as ever.
        if let Some(h) = snapshot.history_id
            && self.history_id.is_none()
            && !self.labels.is_empty()
        {
            self.history_id = Some(h);
            self.synced_once = true;
            self.resumed = true;
        }
        true
    }

    /// Parks a background job until the backoff passes.
    pub(super) fn defer(&mut self, job: Job) {
        if !self.deferred.contains(&job) {
            self.deferred.push(job);
        }
    }

    /// SPEC 7.4 step 2: the cursor moves together with the work it implies, in one step.
    pub(super) fn commit_round(&mut self, changes: Changes) {
        self.pending_changed.extend(changes.changed);
        self.pending_fresh.extend(changes.fresh_inbox);
        self.round_labels.extend(changes.labels);
        if let Some(h) = changes.new_history_id {
            self.history_id = Some(h);
        }
    }

    /// The next chunk of changed threads to refetch, new inbox mail first. A burst of more
    /// than [`BURST`] keeps only the newest [`BURST_FRESH`] new inbox threads and drops the
    /// rest from the cache, so a later listing fetches them instead.
    pub(super) fn next_refetch(&mut self) -> Vec<String> {
        if self.pending_changed.len() > BURST {
            let mut fresh: Vec<String> = self
                .pending_fresh
                .iter()
                .filter(|id| self.pending_changed.contains(*id))
                .cloned()
                .collect();
            // Gmail thread ids grow with time, so the longest, then greatest, is the newest.
            fresh.sort_by(|a, b| (b.len(), b).cmp(&(a.len(), a)));
            fresh.truncate(BURST_FRESH);
            let keep: BTreeSet<String> = fresh.into_iter().collect();
            for id in &self.pending_changed {
                if !keep.contains(id) {
                    self.known.remove(id);
                }
            }
            self.pending_fresh.retain(|id| keep.contains(id));
            self.pending_changed = keep;
        }
        let mut ids: Vec<String> = self
            .pending_fresh
            .iter()
            .filter(|id| self.pending_changed.contains(*id))
            .take(CHUNK)
            .cloned()
            .collect();
        for id in &self.pending_changed {
            if ids.len() >= CHUNK {
                break;
            }
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    /// A changed thread was refetched.
    pub(super) fn refetch_succeeded(&mut self, id: &str) {
        self.pending_changed.remove(id);
    }

    /// A changed thread is gone from Gmail: nothing to refetch or report.
    pub(super) fn refetch_gone(&mut self, id: &str) {
        self.pending_changed.remove(id);
        self.pending_fresh.remove(id);
        self.known.remove(id);
        self.missing.remove(id);
        self.refused.remove(id);
        self.dirty = true;
    }

    /// SPEC 7.4 step 7, once every changed thread is refetched: the candidates still unread
    /// and in the inbox, newest first. Every candidate is cleared, reported or not.
    pub(super) fn take_new_mail(&mut self) -> Vec<ThreadSummary> {
        let mut threads: Vec<ThreadSummary> = std::mem::take(&mut self.pending_fresh)
            .iter()
            .filter_map(|id| self.known.get(id))
            .map(|k| &k.summary)
            .filter(|s| s.unread() && s.in_label(labels::INBOX))
            .cloned()
            .collect();
        threads.sort_by_key(|s| std::cmp::Reverse(s.last_date));
        threads
    }

    /// SPEC 7.4 step 8: the history is too old to follow, so every cache goes and the next
    /// sync is a first one again (which never reports new mail).
    pub(super) fn reset(&mut self) {
        self.known.clear();
        self.pages.clear();
        self.history_id = None;
        self.synced_once = false;
        self.pending_changed.clear();
        self.pending_fresh.clear();
        self.round_labels.clear();
        self.missing.clear();
        self.refused.clear();
        self.listed.clear();
        self.resumed = false;
        self.dirty = true;
        self.deferred
            .retain(|j| !matches!(j, Job::FetchMeta(..) | Job::Refetch(_) | Job::History(_)));
    }
}

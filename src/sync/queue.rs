use super::*;

/// What a [`Mutation`] does to each thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Modify(LabelDelta),
    Trash,
    /// Untrash, then apply the delta (when not empty) to put the labels back.
    Untrash(LabelDelta),
}

/// One optimistic change the store is waiting to hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Mutation {
    pub(super) op: OpId,
    pub(super) account: AccountId,
    pub(super) thread_ids: Vec<String>,
    pub(super) change: Change,
}

/// A message to send. Two are the same job when they share an op id (`Outgoing` has no
/// equality of its own, and its `Debug` is redacting).
#[derive(Clone, Debug)]
pub(super) struct Outbox {
    pub(super) op: OpId,
    pub(super) outgoing: Outgoing,
}

impl PartialEq for Outbox {
    fn eq(&self, other: &Self) -> bool {
        self.op == other.op
    }
}

impl Eq for Outbox {}

/// Work on a draft. Saves and loads are the same job when they share an op id; a delete when it
/// names the same draft.
#[derive(Clone)]
pub(super) enum DraftWork {
    Save {
        op: OpId,
        outgoing: Box<Outgoing>,
        draft_id: Option<String>,
    },
    Delete {
        account: AccountId,
        draft_id: String,
    },
    Load {
        op: OpId,
        account: AccountId,
        thread_id: String,
    },
}

impl PartialEq for DraftWork {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Save { op: a, .. }, Self::Save { op: b, .. })
            | (Self::Load { op: a, .. }, Self::Load { op: b, .. }) => a == b,
            (
                Self::Delete {
                    account: a,
                    draft_id: x,
                },
                Self::Delete {
                    account: b,
                    draft_id: y,
                },
            ) => a == b && x == y,
            _ => false,
        }
    }
}

impl Eq for DraftWork {}

impl std::fmt::Debug for DraftWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Save { .. } => "DraftWork::Save",
            Self::Delete { .. } => "DraftWork::Delete",
            Self::Load { .. } => "DraftWork::Load",
        })
    }
}

/// An attachment to save (SPEC 7.5 `SaveAttachment`). Two are the same job when they name the
/// same part of the same message; `Debug` leaves the filename out.
#[derive(Clone)]
pub(super) struct Download {
    pub(super) account: AccountId,
    pub(super) message_id: String,
    pub(super) attachment: Attachment,
}

impl PartialEq for Download {
    fn eq(&self, other: &Self) -> bool {
        self.account == other.account
            && self.message_id == other.message_id
            && self.attachment.part_id == other.attachment.part_id
    }
}

impl Eq for Download {}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("message_id", &self.message_id)
            .field("part_id", &self.attachment.part_id)
            .finish_non_exhaustive()
    }
}

/// One unit of work (SPEC 7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Job {
    // Interactive lane: the front of the queue, FIFO among themselves.
    OpenThread(ThreadKey),
    /// A view's page 1 (`false`) or next page (`true`).
    ListView(ViewKey, bool),
    Mutate(Mutation),
    Send(Box<Outbox>),
    Draft(Box<DraftWork>),
    Save(Box<Download>),
    // Background lane: the back of the queue, coalesced.
    InitialSync(AccountId),
    /// Label ids whose counts to fetch, at most [`CHUNK`].
    Labels(AccountId, Vec<String>),
    /// Thread ids whose metadata to fetch, at most [`CHUNK`].
    FetchMeta(AccountId, Vec<String>),
    History(AccountId),
    /// The next chunk of a history round's changed threads (SPEC 7.4 step 4).
    Refetch(AccountId),
    /// A new thread's full detail for a background AI check (SPEC 16.11).
    Prefetch(ThreadKey),
}

impl Job {
    pub(super) fn interactive(&self) -> bool {
        matches!(
            self,
            Job::OpenThread(_)
                | Job::ListView(..)
                | Job::Mutate(_)
                | Job::Send(_)
                | Job::Draft(_)
                | Job::Save(_)
        )
    }

    pub(super) fn account(&self) -> &str {
        match self {
            Job::OpenThread(key) | Job::Prefetch(key) => &key.account,
            Job::ListView(view, _) => &view.account,
            Job::Mutate(m) => &m.account,
            Job::Send(s) => &s.outgoing.account,
            Job::Draft(d) => match &**d {
                DraftWork::Save { outgoing, .. } => &outgoing.account,
                DraftWork::Delete { account, .. } | DraftWork::Load { account, .. } => account,
            },
            Job::Save(d) => &d.account,
            Job::InitialSync(a)
            | Job::Labels(a, _)
            | Job::FetchMeta(a, _)
            | Job::History(a)
            | Job::Refetch(a) => a,
        }
    }

    /// Whether this job shows the busy dot.
    pub(super) fn shows_busy(&self) -> bool {
        matches!(
            self,
            Job::ListView(..) | Job::InitialSync(_) | Job::History(_)
        )
    }
}

/// The job queue's two lanes.
#[derive(Debug, Default)]
pub(super) struct Queue {
    pub(super) interactive: VecDeque<Job>,
    pub(super) background: VecDeque<Job>,
}

impl Queue {
    /// Queues `job` at the back of its lane, unless the same job is already waiting.
    pub(super) fn push(&mut self, job: Job) {
        let lane = if job.interactive() {
            &mut self.interactive
        } else {
            &mut self.background
        };
        if !lane.contains(&job) {
            lane.push_back(job);
        }
    }

    /// Queues background `jobs` at the front of the background lane, in order: the metadata
    /// of a page someone is looking at comes before label counts.
    pub(super) fn push_soon(&mut self, jobs: Vec<Job>) {
        for job in jobs.into_iter().rev() {
            debug_assert!(!job.interactive());
            if !self.background.contains(&job) {
                self.background.push_front(job);
            }
        }
    }

    pub(super) fn pop(&mut self) -> Option<Job> {
        self.interactive
            .pop_front()
            .or_else(|| self.background.pop_front())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.interactive.is_empty() && self.background.is_empty()
    }

    /// The thread ids already waiting in `account`'s queued metadata chunks.
    pub(super) fn queued_meta(&self, account: &str) -> HashSet<String> {
        meta_ids(self.background.iter(), account)
    }

    /// Takes `ids` out of `account`'s queued metadata chunks (dropping chunks left empty),
    /// so a fresh listing can queue them again at the front without fetching them twice.
    pub(super) fn strip_meta(&mut self, account: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let ids: HashSet<&String> = ids.iter().collect();
        self.background.retain_mut(|job| match job {
            Job::FetchMeta(a, chunk) if a == account => {
                chunk.retain(|id| !ids.contains(id));
                !chunk.is_empty()
            }
            _ => true,
        });
    }

    /// Drops every job of a signed-out account.
    pub(super) fn forget(&mut self, account: &str) {
        self.interactive.retain(|j| j.account() != account);
        self.background.retain(|j| j.account() != account);
    }
}

/// The thread ids in `account`'s metadata chunks among `jobs`.
pub(super) fn meta_ids<'a>(jobs: impl Iterator<Item = &'a Job>, account: &str) -> HashSet<String> {
    jobs.filter_map(|j| match j {
        Job::FetchMeta(a, ids) if a == account => Some(ids),
        _ => None,
    })
    .flatten()
    .cloned()
    .collect()
}

/// Splits `ids` into jobs of at most [`CHUNK`] ids.
pub(super) fn chunked(ids: &[String], job: impl Fn(Vec<String>) -> Job) -> Vec<Job> {
    ids.chunks(CHUNK).map(|c| job(c.to_vec())).collect()
}

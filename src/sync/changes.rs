use super::*;

pub(super) struct Access {
    pub(super) token: String,
    pub(super) expires: Instant,
}

/// What the worker last fetched of a thread.
pub(super) struct Known {
    pub(super) history_id: u64,
    /// The message a star goes on: the last one that is not a draft (SPEC 6.2).
    pub(super) star_target: Option<String>,
    pub(super) summary: ThreadSummary,
}

/// What one history round changed (SPEC 7.4 step 1), from [`collect_changes`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Changes {
    /// Every thread a message was added to, deleted from, or relabelled in.
    pub(super) changed: BTreeSet<String>,
    /// Threads with a new unread inbox message Gmail would notify about.
    pub(super) fresh_inbox: BTreeSet<String>,
    /// Labels added to or removed from a message, whose counts moved.
    pub(super) labels: BTreeSet<String>,
    /// The cursor for the next round: the last page's history id.
    pub(super) new_history_id: Option<u64>,
}

/// Folds history pages into the threads to refetch and the new inbox mail (SPEC 7.4). Pure.
pub(super) fn collect_changes(pages: &[gmail::HistoryList]) -> Changes {
    let mut c = Changes::default();
    let mut changed = |thread_id: &str| {
        if !thread_id.is_empty() {
            c.changed.insert(thread_id.to_string());
        }
    };
    let mut fresh = Vec::new();
    let mut moved = Vec::new();
    for record in pages.iter().flat_map(|p| &p.history) {
        for m in &record.messages {
            changed(&m.thread_id);
        }
        for added in &record.messages_added {
            let m = &added.message;
            changed(&m.thread_id);
            let has = |l: &str| m.label_ids.iter().any(|id| id == l);
            if has(labels::INBOX)
                && has(labels::UNREAD)
                && !NOT_NOTIFIED.iter().any(|l| has(l))
                && !m.thread_id.is_empty()
            {
                fresh.push(m.thread_id.clone());
            }
        }
        for deleted in &record.messages_deleted {
            changed(&deleted.message.thread_id);
        }
        for l in record.labels_added.iter().chain(&record.labels_removed) {
            changed(&l.message.thread_id);
            moved.extend(l.label_ids.iter().cloned());
        }
    }
    c.fresh_inbox.extend(fresh);
    c.labels.extend(moved);
    c.new_history_id = pages
        .last()
        .map(|p| parse::parse_u64(&p.history_id))
        .filter(|h| *h > 0);
    c
}

/// Whether a delta is exactly "star", which goes on one message rather than the thread.
pub(super) fn is_star(delta: &LabelDelta) -> bool {
    delta.remove.is_empty() && delta.add == [labels::STARRED]
}

/// The message a star goes on: the last one that is not a draft.
pub(super) fn star_target(dto: &gmail::ThreadDto) -> Option<String> {
    dto.messages
        .iter()
        .rev()
        .find(|m| !m.label_ids.iter().any(|l| l == labels::DRAFT))
        .map(|m| m.id.clone())
}

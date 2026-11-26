use super::*;

impl Store {
    // ----- queries -----

    pub(super) fn current_ids(&self) -> Vec<String> {
        self.view
            .as_ref()
            .map(|v| self.shown_ids(v))
            .unwrap_or_default()
    }

    /// The current view's list state.
    pub fn current_list(&self) -> Option<&ListState> {
        self.view.as_ref().and_then(|v| self.lists.get(v))
    }

    /// The current list's rows; ids without a summary are skeletons.
    pub fn visible_rows(&self) -> Vec<Row<'_>> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        let ids = self.shown_ids(view);
        ids.into_iter()
            .map(|id| {
                let key = ThreadKey::new(view.account.as_str(), id.as_str());
                match self.threads.get(&key) {
                    Some(s) => Row::Thread(s),
                    None => Row::Skeleton(key),
                }
            })
            .collect()
    }

    /// The sidebar entries for an account: the system labels in Gmail's order (with All Mail
    /// after Drafts), then visible user labels sorted by name and nested on `/`.
    pub fn labels_for(&self, account: &str) -> Vec<SidebarEntry> {
        let all = self.labels.get(account).map(Vec::as_slice).unwrap_or(&[]);
        let mut out = Vec::new();
        for (id, name, _) in SYSTEM_LABELS {
            if *id == labels::SPAM {
                out.push(SidebarEntry {
                    view: ViewKind::AllMail,
                    name: "All Mail".into(),
                    depth: 0,
                    count: None,
                    color: None,
                });
            }
            out.push(SidebarEntry {
                view: ViewKind::Label((*id).to_string()),
                name: (*name).to_string(),
                depth: 0,
                count: self.unread_badge(account, id),
                color: None,
            });
            if *id == labels::INBOX {
                out.push(SidebarEntry {
                    view: ViewKind::Nudges,
                    name: "Nudges".into(),
                    depth: 0,
                    count: Some(self.nudge_count(account)).filter(|n| *n > 0),
                    color: None,
                });
            }
        }
        let mut user: Vec<&Label> = all
            .iter()
            .filter(|l| l.kind == LabelKind::User && !l.hidden)
            .collect();
        // Compare path components so descendants remain adjacent to their parent, even
        // when another label shares its name as a prefix (e.g. Projects-old).
        user.sort_by_cached_key(|l| l.name.split('/').map(str::to_lowercase).collect::<Vec<_>>());
        let names: HashSet<&str> = user.iter().map(|l| l.name.as_str()).collect();
        for l in user {
            // Nest under the longest existing ancestor; without one, show the full name.
            let mut depth = 0;
            let mut shown = l.name.as_str();
            let mut prefix_end = 0;
            for (i, _) in l.name.match_indices('/') {
                if names.contains(&l.name[..i]) {
                    depth += 1;
                    prefix_end = i + 1;
                }
            }
            if prefix_end > 0 {
                shown = &l.name[prefix_end..];
            }
            out.push(SidebarEntry {
                view: ViewKind::Label(l.id.clone()),
                name: shown.to_string(),
                depth,
                count: self.unread_badge(account, &l.id),
                color: l.color,
            });
        }
        out
    }

    /// The count a sidebar label shows, per SPEC 4.1; `None` when it shows none or it is zero.
    pub fn unread_badge(&self, account: &str, label: &str) -> Option<u32> {
        let l = self.labels.get(account)?.iter().find(|l| l.id == label)?;
        let count = match SYSTEM_LABELS.iter().find(|(id, _, _)| *id == label) {
            Some((_, _, Count::Unread)) => l.threads_unread,
            Some((_, _, Count::Total)) => l.threads_total,
            Some((_, _, Count::None)) => 0,
            None if l.kind == LabelKind::User => l.threads_unread,
            None => 0,
        };
        (count > 0).then_some(count)
    }

    /// The cursor's row index in the current list.
    pub fn cursor_index(&self) -> Option<usize> {
        let cursor = self.cursor.as_ref()?;
        let view = self.view.as_ref()?;
        if cursor.account != view.account {
            return None;
        }
        self.shown_ids(view).iter().position(|id| *id == cursor.id)
    }

    /// The sender label for a thread row, with the thread's account as "me".
    pub fn participants_label(&self, s: &ThreadSummary) -> ParticipantsLabel {
        address::participants_label(&s.participants, &s.key.account, s.message_count)
    }

    /// What a mutation acts on: the selection, else the open thread, else the cursor row.
    pub fn targets(&self) -> Vec<ThreadKey> {
        if !self.selected.is_empty() {
            self.selected.iter().cloned().collect()
        } else if let Some(open) = &self.open {
            vec![open.clone()]
        } else {
            self.cursor.iter().cloned().collect()
        }
    }

    /// Whether a mutation is waiting for the worker.
    pub fn is_pending(&self, key: &ThreadKey) -> bool {
        self.pending
            .values()
            .any(|p| p.threads.iter().any(|s| s.key == *key))
    }

    pub fn send_state(&self, op: OpId) -> Option<&SendState> {
        self.sends.get(&op)
    }

    /// The compose window that started `op` has seen its outcome: forget it.
    pub fn forget_send(&mut self, op: OpId) {
        self.sends.remove(&op);
    }

    /// The compose window that started `op` closed: a send still in flight reports its
    /// outcome as a notice instead, and a settled one is forgotten.
    pub fn abandon_send(&mut self, op: OpId) {
        if matches!(
            self.sends.get(&op),
            Some(SendState::Sending | SendState::Scheduled)
        ) {
            self.abandoned.insert(op);
        }
        self.sends.remove(&op);
    }

    /// Hands new-mail summaries to the notifier and forgets them.
    pub fn take_new_mail(&mut self) -> Vec<ThreadSummary> {
        std::mem::take(&mut self.new_mail)
    }
}

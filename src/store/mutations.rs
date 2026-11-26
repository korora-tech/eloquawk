use super::*;

impl Store {
    // ----- mutations -----

    /// Applies `kind` to the targets (selection, open thread or cursor row) optimistically
    /// and sends it to the worker. Returns the op, or `None` when there was nothing to act on.
    pub fn mutate(&mut self, kind: MutationKind) -> Option<OpId> {
        let targets = self.targets();
        self.mutate_keys(targets, kind, true, None)
    }

    /// Applies `kind` to the given threads only (the star on a list row), whatever the
    /// selection holds.
    pub fn mutate_threads(&mut self, keys: Vec<ThreadKey>, kind: MutationKind) -> Option<OpId> {
        self.mutate_keys(keys, kind, true, None)
    }

    /// Every list position a thread holds.
    pub(super) fn positions_of(&self, key: &ThreadKey) -> Vec<(ViewKey, usize)> {
        self.lists
            .iter()
            .filter(|(v, _)| v.account == key.account)
            .filter_map(|(v, l)| {
                l.ids
                    .iter()
                    .position(|id| *id == key.id)
                    .map(|i| (v.clone(), i))
            })
            .collect()
    }

    /// Moves a label's counts from a thread's `before` state to its `after` state.
    pub(super) fn adjust_counts(&mut self, before: &ThreadSummary, after: &ThreadSummary) {
        let Some(account_labels) = self.labels.get_mut(&before.key.account) else {
            return;
        };
        let shift = |v: u32, d: i64| (v as i64 + d).max(0) as u32;
        for l in account_labels.iter_mut() {
            let (b, a) = (before.in_label(&l.id), after.in_label(&l.id));
            let unread = |s: &ThreadSummary, on: bool| i64::from(on && s.unread());
            let msgs = |s: &ThreadSummary, on: bool| if on { s.message_count as i64 } else { 0 };
            l.threads_total = shift(l.threads_total, i64::from(a) - i64::from(b));
            l.threads_unread = shift(l.threads_unread, unread(after, a) - unread(before, b));
            l.messages_total = shift(l.messages_total, msgs(after, a) - msgs(before, b));
        }
    }

    /// Inserts a thread into a list by date, unless it would land past a loaded page that
    /// has more after it.
    pub(super) fn insert_sorted(&mut self, view: &ViewKey, key: &ThreadKey) {
        let Some(date) = self.threads.get(key).map(|s| s.last_date) else {
            return;
        };
        let threads = &self.threads;
        let Some(list) = self.lists.get_mut(view) else {
            return;
        };
        if list.ids.contains(&key.id) {
            return;
        }
        let at = list.ids.iter().position(|id| {
            threads
                .get(&ThreadKey::new(view.account.as_str(), id.as_str()))
                .is_some_and(|s| s.last_date < date)
        });
        match at {
            Some(i) => list.ids.insert(i, key.id.clone()),
            None if !list.has_more => list.ids.push(key.id.clone()),
            None => {}
        }
    }

    /// Puts threads back at recorded list positions, in ascending index order per list.
    pub(super) fn reinsert(&mut self, positions: &[(ThreadKey, Vec<(ViewKey, usize)>)]) {
        let mut per_view: HashMap<ViewKey, Vec<(usize, String)>> = HashMap::new();
        for (key, places) in positions {
            for (view, ix) in places {
                per_view
                    .entry(view.clone())
                    .or_default()
                    .push((*ix, key.id.clone()));
            }
        }
        for (view, mut items) in per_view {
            items.sort();
            if let Some(list) = self.lists.get_mut(&view) {
                for (ix, id) in items {
                    if !list.ids.contains(&id) {
                        let at = ix.min(list.ids.len());
                        list.ids.insert(at, id);
                    }
                }
            }
        }
    }

    /// The core of every mutation. `undoable` posts the undo toast for kinds that have one;
    /// `restore_positions` (undo) puts threads back where they were before the undone op.
    pub(super) fn mutate_keys(
        &mut self,
        keys: Vec<ThreadKey>,
        kind: MutationKind,
        undoable: bool,
        restore_positions: Option<Positions>,
    ) -> Option<OpId> {
        let account = keys.first()?.account.clone();
        let mut keys: Vec<ThreadKey> = keys
            .into_iter()
            .filter(|k| k.account == account && self.threads.contains_key(k))
            .collect();
        keys.dedup();
        if keys.is_empty() {
            return None;
        }
        let op = self.alloc_op();
        let delta = kind.effective();
        let old_ids = self.current_ids();
        let snapshots: Vec<Snapshot> = keys
            .iter()
            .filter_map(|k| {
                Some(Snapshot {
                    key: k.clone(),
                    before: self.threads.get(k)?.clone(),
                    positions: self.positions_of(k),
                    touched: Vec::new(),
                })
            })
            .collect();
        if let Some(positions) = &restore_positions {
            self.reinsert(positions);
        }
        for snap in &snapshots {
            let mut after = snap.before.clone();
            delta.apply_to(&mut after.label_ids);
            self.adjust_counts(&snap.before, &after);
            self.threads.insert(snap.key.clone(), after);
        }
        let views: Vec<ViewKey> = self
            .lists
            .keys()
            .filter(|v| v.account == account)
            .cloned()
            .collect();
        for view in views {
            let all_labels = self.labels.get(&account).map(Vec::as_slice).unwrap_or(&[]);
            if removed_by(&view.kind, &delta, all_labels) {
                if let Some(list) = self.lists.get_mut(&view) {
                    list.ids.retain(|id| !keys.iter().any(|k| k.id == *id));
                }
            } else {
                for key in &keys {
                    let gained = self
                        .threads
                        .get(key)
                        .is_some_and(|s| gained_by(&view.kind, &delta, s));
                    if gained {
                        self.insert_sorted(&view, key);
                    }
                }
            }
        }
        let mut snapshots = snapshots;
        for snap in &mut snapshots {
            let was: HashSet<ViewKey> = snap.positions.iter().map(|(v, _)| v.clone()).collect();
            let now: HashSet<ViewKey> = self
                .positions_of(&snap.key)
                .into_iter()
                .map(|(v, _)| v)
                .collect();
            snap.touched = was.symmetric_difference(&now).cloned().collect();
        }
        let thread_ids: Vec<String> = keys.iter().map(|k| k.id.clone()).collect();
        let n = thread_ids.len();
        self.send_command(match &kind {
            MutationKind::Labels(d) => Command::Modify {
                op,
                account: account.clone(),
                thread_ids,
                delta: d.clone(),
            },
            MutationKind::Trash => Command::Trash {
                op,
                account: account.clone(),
                thread_ids,
            },
            MutationKind::Untrash { restore } => Command::Untrash {
                op,
                account: account.clone(),
                thread_ids,
                restore: restore.clone(),
            },
        });
        if undoable && let Some(text) = kind.undo_text(n) {
            self.undo = Some((
                op,
                UndoAction {
                    kind: kind.clone(),
                    threads: snapshots.clone(),
                },
            ));
            self.post_notice(text, Some(NoticeAction::Undo));
        }
        self.pending.insert(
            op,
            Pending {
                kind,
                account,
                threads: snapshots,
            },
        );
        self.settle(&old_ids, true);
        Some(op)
    }

    /// Reverses the last undoable mutation as a new op. False when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((_, action)) = self.undo.take() else {
            return false;
        };
        let positions: Positions = action
            .threads
            .iter()
            .map(|s| (s.key.clone(), s.positions.clone()))
            .collect();
        let keys: Vec<ThreadKey> = action.threads.iter().map(|s| s.key.clone()).collect();
        match &action.kind {
            MutationKind::Labels(d) => {
                self.mutate_keys(
                    keys,
                    MutationKind::Labels(d.inverse()),
                    false,
                    Some(positions),
                );
            }
            MutationKind::Untrash { .. } => {
                self.mutate_keys(keys, MutationKind::Trash, false, Some(positions));
            }
            MutationKind::Trash => {
                // One untrash per distinct set of labels to restore, so no thread gains a
                // label it never had.
                let mut groups: BTreeMap<Vec<String>, Positions> = BTreeMap::new();
                for s in &action.threads {
                    groups
                        .entry(restorable(&s.before.label_ids))
                        .or_default()
                        .push((s.key.clone(), s.positions.clone()));
                }
                // Each op puts back only its own threads, so a failed group rolls back alone.
                // Positions were recorded with every thread present: an index shrinks by the
                // threads of groups not yet restored that sit before it in the same list.
                let groups: Vec<(Vec<String>, Positions)> = groups.into_iter().collect();
                for (gi, (add, group)) in groups.iter().enumerate() {
                    let later: Vec<&(ViewKey, usize)> = groups[gi + 1..]
                        .iter()
                        .flat_map(|(_, g)| g.iter().flat_map(|(_, places)| places))
                        .collect();
                    let adjusted: Positions = group
                        .iter()
                        .map(|(key, places)| {
                            let places = places
                                .iter()
                                .map(|(view, ix)| {
                                    let before =
                                        later.iter().filter(|(v, i)| v == view && i < ix).count();
                                    (view.clone(), ix - before)
                                })
                                .collect();
                            (key.clone(), places)
                        })
                        .collect();
                    let restore = LabelDelta {
                        add: add.clone(),
                        remove: vec![],
                    };
                    self.mutate_keys(
                        group.iter().map(|(k, _)| k.clone()).collect(),
                        MutationKind::Untrash { restore },
                        false,
                        Some(adjusted),
                    );
                }
            }
        }
        self.post_notice("Action undone".into(), None);
        true
    }

    /// Sends a message; the outcome arrives as `SendFinished`. A reply's thread is
    /// remembered so its detail is fetched again once the message is in it.
    pub fn send(&mut self, outgoing: Outgoing) -> OpId {
        let op = self.alloc_op();
        self.sends.insert(op, SendState::Sending);
        if let Some(id) = &outgoing.thread_id {
            self.send_threads
                .insert(op, ThreadKey::new(outgoing.account.as_str(), id.as_str()));
        }
        self.send_command(Command::Send { op, outgoing });
        op
    }

    /// Like [`Store::send`], but holds the message for the configured undo window so
    /// [`Store::cancel_send`] can take it back. Returns the op and the wait: the caller runs
    /// the timer (on something that outlives the window) and calls
    /// [`Store::dispatch_scheduled`] when it ends. With no delay it is a plain send and the
    /// wait is `None`.
    pub fn send_undoable(&mut self, outgoing: Outgoing) -> (OpId, Option<Duration>) {
        let secs = u64::from(self.prefs.undo_send_secs);
        if secs == 0 {
            return (self.send(outgoing), None);
        }
        let op = self.alloc_op();
        self.sends.insert(op, SendState::Scheduled);
        if let Some(id) = &outgoing.thread_id {
            self.send_threads
                .insert(op, ThreadKey::new(outgoing.account.as_str(), id.as_str()));
        }
        self.scheduled.insert(op, outgoing);
        (op, Some(Duration::from_secs(secs)))
    }

    /// Saves the composer's text as the account's Gmail draft: `draft_id` replaces an existing
    /// one. The outcome is read with [`Store::draft_op`].
    pub fn save_draft(&mut self, outgoing: Outgoing, draft_id: Option<String>) -> OpId {
        let op = self.alloc_op();
        self.drafts.insert(op, DraftOp::Saving);
        self.send_command(Command::SaveDraft {
            op,
            outgoing,
            draft_id,
        });
        op
    }

    /// Deletes a Gmail draft (the message was discarded).
    pub fn delete_draft(&self, account: &str, draft_id: &str) {
        self.send_command(Command::DeleteDraft {
            account: account.to_string(),
            draft_id: draft_id.to_string(),
        });
    }

    /// The message of save `op` was discarded while the save was still running. The store, not
    /// the window (which may be gone), deletes the draft once the save lands: deleting now
    /// would let the save create it again. `known` is the draft's id before the save, deleted
    /// if the save fails.
    pub fn discard_after_save(&mut self, op: OpId, account: &str, known: Option<String>) {
        if matches!(self.drafts.get(&op), Some(DraftOp::Saving)) {
            self.discard_saves.insert(op, (account.to_string(), known));
        } else if let Some(id) = known {
            self.delete_draft(account, &id);
        }
    }

    /// Reads the draft in conversation `thread_id` for editing; the outcome is read with
    /// [`Store::draft_op`].
    pub fn load_draft(&mut self, account: &str, thread_id: &str) -> OpId {
        let op = self.alloc_op();
        self.drafts.insert(op, DraftOp::Loading);
        self.send_command(Command::LoadDraft {
            op,
            account: account.to_string(),
            thread_id: thread_id.to_string(),
        });
        op
    }

    pub fn draft_op(&self, op: OpId) -> Option<&DraftOp> {
        self.drafts.get(&op)
    }

    /// The window that started `op` has read its outcome.
    pub fn forget_draft_op(&mut self, op: OpId) {
        self.drafts.remove(&op);
    }

    /// Ties a Gmail draft to a send: it is deleted when the send succeeds.
    pub fn note_send_draft(&mut self, op: OpId, account: &str, draft_id: String) {
        self.send_drafts.insert(op, (account.to_string(), draft_id));
    }

    /// Takes a message back from the undo window. `None` when it already left (or never was
    /// held), in which case nothing changes.
    pub fn cancel_send(&mut self, op: OpId) -> Option<Outgoing> {
        let outgoing = self.scheduled.remove(&op)?;
        self.sends.remove(&op);
        self.send_threads.remove(&op);
        self.send_drafts.remove(&op);
        self.abandoned.remove(&op);
        Some(outgoing)
    }

    /// The undo window of `op` ended: hands the message to the worker. A message taken back
    /// meanwhile is gone, so this does nothing.
    pub fn dispatch_scheduled(&mut self, op: OpId) {
        let Some(outgoing) = self.scheduled.remove(&op) else {
            return;
        };
        if let Some(state) = self.sends.get_mut(&op) {
            *state = SendState::Sending;
        } else {
            // Its window is gone: the outcome is a notice, as for any abandoned send.
            self.abandoned.insert(op);
        }
        self.send_command(Command::Send { op, outgoing });
    }

    /// Ends every undo window now, for quitting.
    pub fn flush_sends(&mut self) {
        let ops: Vec<OpId> = self.scheduled.keys().copied().collect();
        for op in ops {
            self.dispatch_scheduled(op);
        }
    }

    /// A message is still held, or sent without an outcome yet: quitting now would lose it.
    pub fn sends_unsettled(&self) -> bool {
        !self.scheduled.is_empty()
            || !self.abandoned.is_empty()
            || self.sends.values().any(|s| *s == SendState::Sending)
    }

    /// After the current list changed: auto-advances the reading pane off a removed thread
    /// (when `advance`), moves a removed cursor to the row at its old index, and drops
    /// selected threads that left the list.
    pub(super) fn settle(&mut self, old_ids: &[String], advance: bool) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let new_ids = self.current_ids();
        let in_new = |id: &str| new_ids.iter().any(|n| n == id);
        if advance
            && let Some(open) = self.open.clone()
            && open.account == view.account
            && !in_new(&open.id)
            && let Some(ix) = old_ids.iter().position(|id| *id == open.id)
        {
            match successor(old_ids, &new_ids, ix) {
                Some(id) => {
                    let key = ThreadKey::new(view.account.as_str(), id.as_str());
                    self.cursor = Some(key.clone());
                    self.open_thread(key);
                }
                None => self.open = None,
            }
        }
        let new_ids = self.current_ids();
        let in_new = |id: &str| new_ids.iter().any(|n| n == id);
        let cursor_ok = self
            .cursor
            .as_ref()
            .is_some_and(|c| c.account == view.account && in_new(&c.id));
        if !cursor_ok {
            let old_ix = self
                .cursor
                .as_ref()
                .and_then(|c| old_ids.iter().position(|id| *id == c.id))
                .unwrap_or(0);
            self.cursor = new_ids
                .get(old_ix.min(new_ids.len().saturating_sub(1)))
                .map(|id| ThreadKey::new(view.account.as_str(), id.as_str()));
        }
        self.selected
            .retain(|k| k.account == view.account && in_new(&k.id));
    }
}

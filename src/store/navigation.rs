use super::*;

impl Store {
    // ----- navigation -----

    /// Shows a view: asks the worker for page 1, puts the cursor on the first row, clears the
    /// selection and the reading pane, and remembers a label view in `last_view`.
    pub fn set_view(&mut self, view: ViewKey) {
        self.selected.clear();
        self.open = None;
        self.lists.entry(view.clone()).or_default().loading = true;
        self.cursor = self
            .shown_ids(&view)
            .first()
            .map(|id| ThreadKey::new(view.account.as_str(), id.as_str()));
        let remembered = match &view.kind {
            ViewKind::Label(id) => Some(id.clone()),
            ViewKind::AllMail => Some(ALL_MAIL_ID.to_string()),
            ViewKind::Search(_) | ViewKind::Nudges => None,
        };
        if let Some(id) = remembered {
            let last = Some((view.account.clone(), id));
            if self.prefs.last_view != last {
                self.prefs.last_view = last;
                self.prefs.save();
            }
        }
        self.view = Some(view.clone());
        self.send_command(Command::Want { view, more: false });
    }

    /// Moves the cursor by `delta` rows, clamped to the list. Landing in the last ten rows
    /// asks for the next page when there is one.
    pub fn move_cursor(&mut self, delta: isize) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let ids = self.current_ids();
        if ids.is_empty() {
            return;
        }
        let ix = match self.cursor_index() {
            Some(i) => (i as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
            None => 0,
        };
        self.cursor = Some(ThreadKey::new(view.account.as_str(), ids[ix].as_str()));
        if ix + MORE_THRESHOLD >= ids.len() {
            self.request_more();
        }
    }

    /// Puts the cursor on row `ix` (clamped).
    pub fn set_cursor_index(&mut self, ix: usize) {
        let Some(view) = &self.view else { return };
        let ids = self.current_ids();
        if let Some(id) = ids.get(ix.min(ids.len().saturating_sub(1))) {
            self.cursor = Some(ThreadKey::new(view.account.as_str(), id.as_str()));
        }
    }

    /// Asks for the current view's next page, once.
    pub fn request_more(&mut self) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let Some(list) = self.lists.get_mut(&view) else {
            return;
        };
        if list.has_more && !list.loading {
            list.loading = true;
            self.send_command(Command::Want { view, more: true });
        }
    }

    /// Opens the cursor row in the reading pane.
    pub fn open_cursor(&mut self) {
        if let Some(key) = self.cursor.clone() {
            self.open_thread(key);
        }
    }

    /// The user chose a conversation (Enter, a click). In the Drafts view that edits its draft
    /// instead of reading it; anywhere else it is [`Store::open_thread`].
    pub fn activate(&mut self, key: ThreadKey) {
        if self.in_drafts_view() && self.edit_draft.is_none() {
            self.edit_draft = Some(self.load_draft(&key.account, &key.id));
        } else {
            self.open_thread(key);
        }
    }

    /// [`Store::activate`] on the cursor row.
    pub fn activate_cursor(&mut self) {
        if let Some(key) = self.cursor.clone() {
            self.activate(key);
        }
    }

    fn in_drafts_view(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| matches!(&v.kind, ViewKind::Label(id) if id == labels::DRAFT))
    }

    /// The draft the user opened in the Drafts view, once it has been read: its id and
    /// prefill, or the reason it cannot be edited. `None` while it loads or when none was
    /// asked for.
    pub fn take_edit_draft(&mut self) -> Option<Result<(String, Box<Outgoing>), String>> {
        let op = self.edit_draft?;
        let Some(DraftOp::Loaded(result)) = self.drafts.get(&op) else {
            return None;
        };
        let result = result.clone();
        self.drafts.remove(&op);
        self.edit_draft = None;
        Some(result)
    }

    /// Opens a thread: fetches its detail unless a fresh one is cached, and marks it read
    /// when that preference is on (not undoable, no toast). A cached detail is checked by
    /// the AI at once when the conditions of SPEC 16.10 hold.
    pub fn open_thread(&mut self, key: ThreadKey) {
        self.open = Some(key.clone());
        let summary_history = self.threads.get(&key).map(|s| s.history_id);
        let fresh = match self.details.get(&key) {
            Some(DetailState::Loading) => true,
            Some(DetailState::Loaded(_)) => {
                match (self.detail_history.get(&key), summary_history) {
                    (Some(h), Some(s)) => *h >= s,
                    _ => true,
                }
            }
            Some(DetailState::Failed(_)) | None => false,
        };
        if !fresh {
            self.details.insert(key.clone(), DetailState::Loading);
            self.send_command(Command::OpenThread(key.clone()));
        } else {
            self.check_open(&key);
        }
        self.touch_detail(&key);
        let unread = self.threads.get(&key).is_some_and(|s| s.unread());
        if self.prefs.mark_read_on_open && unread {
            self.mutate_keys(
                vec![key],
                MutationKind::Labels(LabelDelta::read()),
                false,
                None,
            );
        }
    }

    /// Closes the reading pane.
    pub fn close_thread(&mut self) {
        self.open = None;
    }

    pub(super) fn touch_detail(&mut self, key: &ThreadKey) {
        self.detail_order.retain(|k| k != key);
        self.detail_order.push_back(key.clone());
        while self.detail_order.len() > DETAIL_CACHE {
            let Some(oldest) = self.detail_order.pop_front() else {
                break;
            };
            if Some(&oldest) == self.open.as_ref() {
                self.detail_order.push_back(oldest);
                continue;
            }
            self.details.remove(&oldest);
            self.detail_history.remove(&oldest);
        }
    }

    /// Toggles a thread in the x-selection.
    pub fn toggle_select(&mut self, key: ThreadKey) {
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
    }

    pub fn toggle_select_cursor(&mut self) {
        if let Some(key) = self.cursor.clone() {
            self.toggle_select(key);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// The account the current view belongs to, else the first account.
    pub fn current_account(&self) -> Option<AccountId> {
        self.view
            .as_ref()
            .map(|v| v.account.clone())
            .or_else(|| self.accounts.first().cloned())
    }

    /// Shows a label (or All Mail) of the current account: the `g` keys.
    pub fn go_to(&mut self, kind: ViewKind) {
        if let Some(account) = self.current_account() {
            self.set_view(ViewKey { account, kind });
        }
    }

    /// Switches to the inbox of the account `delta` places along, wrapping round: `g n` and
    /// `g p`. With one account it shows that account's inbox.
    pub fn switch_account(&mut self, delta: isize) {
        let n = self.accounts.len() as isize;
        if n == 0 {
            return;
        }
        let at = self
            .view
            .as_ref()
            .and_then(|v| self.accounts.iter().position(|a| *a == v.account))
            .map_or(0, |i| i as isize + delta);
        let account = self.accounts[at.rem_euclid(n) as usize].clone();
        self.set_view(ViewKey {
            account,
            kind: ViewKind::Label(labels::INBOX.into()),
        });
    }

    // ----- search -----

    /// Shows the results of `query` (sent to Gmail verbatim, trimmed) for the current account,
    /// remembering the view it replaces for [`Store::end_search`]. A query with no terms ends
    /// the search instead.
    pub fn search(&mut self, query: &str) {
        let query = query.trim();
        // Nothing but whitespace or empty quotes: nothing to search for.
        if Query::parse(query).is_empty() {
            self.end_search();
            return;
        }
        let Some(account) = self.current_account() else {
            return;
        };
        if let Some(view) = &self.view
            && !matches!(view.kind, ViewKind::Search(_))
        {
            self.search_return = Some(view.clone());
        }
        self.set_view(ViewKey {
            account,
            kind: ViewKind::Search(query.to_string()),
        });
    }

    /// The query of the search being shown.
    pub fn search_query(&self) -> Option<&str> {
        match &self.view.as_ref()?.kind {
            ViewKind::Search(q) => Some(q),
            _ => None,
        }
    }

    /// Leaves a search for the view it replaced (the account's inbox when that view belonged
    /// to another account or is unknown). False when no search is shown.
    pub fn end_search(&mut self) -> bool {
        let Some(view) = self.view.clone() else {
            return false;
        };
        if !matches!(view.kind, ViewKind::Search(_)) {
            return false;
        }
        let back = self
            .search_return
            .take()
            .filter(|v| v.account == view.account)
            .unwrap_or(ViewKey {
                account: view.account,
                kind: ViewKind::Label(labels::INBOX.into()),
            });
        self.set_view(back);
        true
    }
}

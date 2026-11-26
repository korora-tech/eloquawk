use super::*;
use crate::nudges::{self, Dismissed, Nudge};

impl Store {
    // ----- nudges (openspec/specs/nudges) -----

    /// What kind of nudge `s` is right now, if any.
    pub fn nudge_of(&self, s: &ThreadSummary) -> Option<Nudge> {
        self.nudge_at(s, Timestamp::now())
    }

    pub(super) fn nudge_at(&self, s: &ThreadSummary, now: Timestamp) -> Option<Nudge> {
        let none = Dismissed::new();
        let dismissed = self.nudge_dismissed.get(&s.key.account).unwrap_or(&none);
        nudges::classify(s, now, dismissed)
    }

    /// The ids a list shows. The Nudges list is a search narrowed to the real nudges, and the
    /// Inbox leaves those out so they show only in the Nudges list. A row whose summary has
    /// not arrived stays as a skeleton until it does.
    pub(super) fn shown_ids(&self, view: &ViewKey) -> Vec<String> {
        let Some(list) = self.lists.get(view) else {
            return Vec::new();
        };
        let now = Timestamp::now();
        let inbox = view.kind == ViewKind::Label(labels::INBOX.into());
        if view.kind != ViewKind::Nudges && !inbox {
            return list.ids.clone();
        }
        list.ids
            .iter()
            .filter(|id| {
                let nudge = self
                    .threads
                    .get(&ThreadKey::new(view.account.as_str(), id.as_str()))
                    .map(|s| self.nudge_at(s, now).is_some());
                // The inbox leaves nudges to their own list; a skeleton row stays in either.
                match nudge {
                    Some(is_nudge) => is_nudge != inbox,
                    None => true,
                }
            })
            .cloned()
            .collect()
    }

    /// How many loaded threads of the account are nudges, for the sidebar badge.
    pub fn nudge_count(&self, account: &str) -> u32 {
        let now = Timestamp::now();
        let n = self
            .threads
            .values()
            .filter(|s| s.key.account == account && self.nudge_at(s, now).is_some())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    pub fn viewing_nudges(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.kind == ViewKind::Nudges)
    }

    /// Dismisses the targets' nudges: they leave the Nudges list and stay out until a new
    /// message arrives in the thread. The worker keeps the dismissal in encrypted storage.
    /// Returns how many were dismissed.
    pub fn dismiss_nudges(&mut self) -> usize {
        let targets = self.targets();
        self.dismiss_keys(targets)
    }

    /// Dismisses one thread's nudge, whatever the selection holds (a row's button).
    pub fn dismiss_nudge_of(&mut self, key: ThreadKey) -> usize {
        self.dismiss_keys(vec![key])
    }

    fn dismiss_keys(&mut self, keys: Vec<ThreadKey>) -> usize {
        let now = Timestamp::now();
        let entries: Vec<(ThreadKey, i64)> = keys
            .into_iter()
            .filter_map(|k| {
                let s = self.threads.get(&k)?;
                self.nudge_at(s, now)?;
                Some((k, s.last_date.as_millisecond()))
            })
            .collect();
        if entries.is_empty() {
            return 0;
        }
        let old_ids = self.current_ids();
        let mut by_account: BTreeMap<AccountId, Vec<(String, i64)>> = BTreeMap::new();
        for (key, date) in &entries {
            self.nudge_dismissed
                .entry(key.account.clone())
                .or_default()
                .insert(key.id.clone(), *date);
            by_account
                .entry(key.account.clone())
                .or_default()
                .push((key.id.clone(), *date));
        }
        for (account, entries) in by_account {
            self.send_command(Command::DismissNudges { account, entries });
        }
        self.selected.clear();
        self.settle(&old_ids, true);
        let n = entries.len();
        self.post_notice(
            if n == 1 {
                "Nudge dismissed".to_string()
            } else {
                format!("{n} nudges dismissed")
            },
            None,
        );
        n
    }

    /// The worker's copy of an account's dismissals, read at start and after a change.
    pub(super) fn on_nudge_dismissals(&mut self, account: AccountId, dismissed: Dismissed) {
        let old_ids = self.current_ids();
        self.nudge_dismissed.insert(account, dismissed);
        self.settle(&old_ids, false);
    }
}

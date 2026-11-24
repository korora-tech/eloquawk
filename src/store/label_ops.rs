use super::*;

impl Store {
    // ----- labels -----

    /// A label's name in an account, or its id when the label is unknown.
    pub fn label_name(&self, account: &str, id: &str) -> String {
        self.labels
            .get(account)
            .and_then(|ls| ls.iter().find(|l| l.id == id))
            .map_or_else(|| id.to_string(), |l| l.name.clone())
    }

    /// Adds (`on`) or removes a label on the given threads: the label picker's toggle. It is
    /// undoable, and the toast names the label.
    pub fn set_label(&mut self, keys: Vec<ThreadKey>, label: &str, on: bool) -> Option<OpId> {
        let account = keys.first()?.account.clone();
        let delta = if on {
            LabelDelta {
                add: vec![label.to_string()],
                remove: vec![],
            }
        } else {
            LabelDelta {
                add: vec![],
                remove: vec![label.to_string()],
            }
        };
        let op = self.mutate_keys(keys, MutationKind::Labels(delta), true, None)?;
        let n = self.pending.get(&op).map_or(0, |p| p.threads.len());
        let what = if n == 1 {
            "1 conversation".to_string()
        } else {
            format!("{n} conversations")
        };
        let name = self.label_name(&account, label);
        if let Some(notice) = &mut self.notice
            && notice.action == Some(NoticeAction::Undo)
        {
            notice.text = if on {
                format!("Added \u{201c}{name}\u{201d} to {what}")
            } else {
                format!("Removed \u{201c}{name}\u{201d} from {what}")
            };
        }
        Some(op)
    }

    // ----- view-aware actions (keys and the reader's action bar) -----

    /// Whether the current view is this label.
    pub fn viewing_label(&self, id: &str) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.kind == ViewKind::Label(id.to_string()))
    }

    /// `e` and Archive: removes INBOX, or does Not spam in the Spam view.
    pub fn archive_kind(&self) -> MutationKind {
        if self.viewing_label(labels::SPAM) {
            MutationKind::Labels(LabelDelta::not_spam())
        } else {
            MutationKind::Labels(LabelDelta::archive())
        }
    }

    /// `#` and Trash: moves to Trash, or restores to the inbox in the Trash view.
    pub fn trash_kind(&self) -> MutationKind {
        if self.viewing_label(labels::TRASH) {
            MutationKind::Untrash {
                restore: LabelDelta {
                    add: vec![labels::INBOX.into()],
                    remove: vec![],
                },
            }
        } else {
            MutationKind::Trash
        }
    }

    /// `!`: reports spam, or does Not spam in the Spam view.
    pub fn spam_kind(&self) -> MutationKind {
        if self.viewing_label(labels::SPAM) {
            MutationKind::Labels(LabelDelta::not_spam())
        } else {
            MutationKind::Labels(LabelDelta::spam())
        }
    }

    /// `s` and Star: stars the targets unless every one is already starred, which unstars.
    pub fn star_kind(&self) -> MutationKind {
        let targets = self.targets();
        let all_starred = !targets.is_empty()
            && targets
                .iter()
                .all(|k| self.threads.get(k).is_some_and(ThreadSummary::starred));
        if all_starred {
            MutationKind::Labels(LabelDelta::unstar())
        } else {
            MutationKind::Labels(LabelDelta::star())
        }
    }
}

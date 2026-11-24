use super::*;

impl Worker {
    /// Queues a change, or answers at once for an account that is not signed in.
    pub(super) fn queue_mutation(&mut self, m: Mutation) {
        if self.accounts.contains_key(&m.account) {
            self.queue.push(Job::Mutate(m));
        } else {
            self.emit(SyncEvent::MutationFinished {
                op: m.op,
                error: Some(SIGNED_OUT.into()),
            });
        }
    }

    /// SPEC 7.5 `Mutate`: thread by thread, stopping at the first failure. Never retried
    /// (bar the one token refresh); the store rolls back on an error. The touched threads'
    /// metadata is then fetched again, which also corrects a change that stopped part-way,
    /// and the counts of the labels involved are refreshed.
    pub(super) fn mutate(&mut self, m: Mutation) {
        let email = m.account.clone();
        let mut carried: BTreeSet<String> = CHANGE_COUNTS.iter().map(|s| s.to_string()).collect();
        if let Some(account) = self.accounts.get(&email) {
            for id in &m.thread_ids {
                if let Some(k) = account.known.get(id) {
                    carried.extend(k.summary.label_ids.iter().cloned());
                }
            }
        }
        let mut error = None;
        let mut touched = false;
        for id in &m.thread_ids {
            let result = match &m.change {
                Change::Modify(delta) if is_star(delta) => self.star(&email, id),
                Change::Modify(delta) => self.with_token(&email, |agent, ep, token| {
                    gmail::modify_thread(agent, ep, token, id, delta)
                }),
                Change::Trash => self
                    .with_token(&email, |agent, ep, token| {
                        gmail::trash(agent, ep, token, id)
                    })
                    .map(drop),
                Change::Untrash(restore) => self
                    .with_token(&email, |agent, ep, token| {
                        gmail::untrash(agent, ep, token, id)
                    })
                    .and_then(|_| {
                        // Out of Trash already: a failed restore below still needs the
                        // refetch to show the thread as Gmail now has it.
                        touched = true;
                        if restore.is_empty() {
                            return Ok(());
                        }
                        self.with_token(&email, |agent, ep, token| {
                            gmail::modify_thread(agent, ep, token, id, restore)
                        })
                    }),
            };
            match result {
                Ok(()) => {
                    touched = true;
                    self.succeeded(&email);
                }
                Err(e) => {
                    error = Some(change_message(&e));
                    self.failed(&email, &e);
                    break;
                }
            }
        }
        self.emit(SyncEvent::MutationFinished { op: m.op, error });
        if !touched {
            return;
        }
        match &m.change {
            Change::Modify(delta) | Change::Untrash(delta) => {
                carried.extend(delta.add.iter().chain(&delta.remove).cloned());
            }
            Change::Trash => {}
        }
        self.queue.strip_meta(&email, &m.thread_ids);
        self.queue.push_soon(chunked(&m.thread_ids, |ids| {
            Job::FetchMeta(email.clone(), ids)
        }));
        self.queue_counts(&email, carried);
    }

    /// Stars the thread's last message that is not a draft, as Gmail web does. A thread the
    /// worker has not fetched yet is fetched first to find it.
    pub(super) fn star(&mut self, email: &str, id: &str) -> Result<(), CallError> {
        let known = self
            .accounts
            .get(email)
            .and_then(|a| a.known.get(id))
            .and_then(|k| k.star_target.clone());
        let target = match known {
            Some(target) => target,
            None => {
                let dto = self.with_token(email, |agent, ep, token| {
                    gmail::thread_meta(agent, ep, token, id)
                })?;
                self.remember(email, &dto);
                match star_target(&dto) {
                    Some(target) => target,
                    // Only drafts: star the thread, so the change still lands.
                    None => {
                        return self.with_token(email, |agent, ep, token| {
                            gmail::modify_thread(agent, ep, token, id, &LabelDelta::star())
                        });
                    }
                }
            }
        };
        self.with_token(email, |agent, ep, token| {
            gmail::modify_message(agent, ep, token, &target, &LabelDelta::star())
        })
        .map(drop)
    }

    /// SPEC 7.5 `Send`: build the message and upload it once. Never retried; a failure says
    /// whether the message may have gone out. A sent message shows up through the history
    /// round queued afterwards.
    pub(super) fn send(&mut self, op: OpId, out: Outgoing) {
        let result = self.deliver(&out);
        let sent = result.is_ok();
        self.emit(SyncEvent::SendFinished {
            op,
            error: result.err(),
        });
        if sent
            && let Some(account) = self.accounts.get(&out.account)
            && account.synced_once
            && account.history_id.is_some()
        {
            self.queue.push(Job::History(out.account.clone()));
        }
    }

    /// Work on a draft: save the composer's text as Gmail's draft, delete one, or read one for
    /// editing. Failures are reported to the composer or ignored (a delete), never retried.
    pub(super) fn draft(&mut self, work: DraftWork) {
        let account = match &work {
            DraftWork::Save { outgoing, .. } => outgoing.account.clone(),
            DraftWork::Delete { account, .. } | DraftWork::Load { account, .. } => account.clone(),
        };
        match work {
            DraftWork::Save {
                op,
                outgoing,
                draft_id,
            } => match self.save_draft(&outgoing, draft_id.as_deref()) {
                Ok(draft_id) => {
                    self.succeeded(&account);
                    self.emit(SyncEvent::DraftSaved { op, draft_id });
                    self.queue_history_after_draft(&account);
                }
                Err(message) => self.emit(SyncEvent::DraftFailed { op, message }),
            },
            DraftWork::Delete { draft_id, .. } => {
                let result = self.with_token(&account, |agent, ep, token| {
                    gmail::draft_delete(agent, ep, token, &draft_id)
                });
                // Already gone is as good as deleted.
                if matches!(result, Ok(()) | Err(CallError::Api(ApiError::NotFound))) {
                    self.queue_history_after_draft(&account);
                }
            }
            DraftWork::Load { op, thread_id, .. } => {
                let result = self.load_draft(&account, &thread_id);
                self.emit(SyncEvent::DraftLoaded { op, result });
            }
        }
    }

    /// Lets the Drafts view and counts catch up with a draft change.
    fn queue_history_after_draft(&mut self, account: &str) {
        if let Some(a) = self.accounts.get(account)
            && a.synced_once
            && a.history_id.is_some()
        {
            self.queue.push(Job::History(account.to_string()));
        }
    }

    /// Uploads `out` as a draft: an update of `draft_id` when there is one (a draft gone
    /// meanwhile is created afresh), else a new draft. Returns the draft's id.
    fn save_draft(&mut self, out: &Outgoing, draft_id: Option<&str>) -> Result<String, String> {
        let parts = gather_attachments(out, |message_id, part_id| {
            let data = self
                .attachment_base64(&out.account, message_id, part_id)
                .map_err(|e| attachment_message(&e))?;
            b64::decode(&data).map_err(|_| ATTACHMENT_UNREADABLE.to_string())
        })
        .map_err(|e| e.message)?;
        let message_id = compose::new_message_id();
        let raw = compose::build_draft(out, &parts, Timestamp::now(), &message_id)
            .map_err(|e| e.to_string())?;
        drop(parts);
        let thread_id = out.thread_id.as_deref();
        if let Some(id) = draft_id {
            match self.with_token(&out.account, |agent, ep, token| {
                gmail::draft_update(agent, ep, token, id, &raw, thread_id)
            }) {
                Ok(d) => return Ok(d.id),
                Err(CallError::Api(ApiError::NotFound)) => {}
                Err(e) => return Err(change_message(&e)),
            }
        }
        self.with_token(&out.account, |agent, ep, token| {
            gmail::draft_create(agent, ep, token, &raw, thread_id)
        })
        .map(|d| d.id)
        .map_err(|e| change_message(&e))
    }

    /// Finds the draft in conversation `thread_id` and reads it back as a composer prefill.
    /// A draft with attachments is refused: they would be lost on the next save.
    fn load_draft(
        &mut self,
        account: &str,
        thread_id: &str,
    ) -> Result<(String, Box<Outgoing>), String> {
        let mut page: Option<String> = None;
        let mut found = None;
        for _ in 0..10 {
            let list = self
                .with_token(account, |agent, ep, token| {
                    gmail::drafts(agent, ep, token, page.as_deref())
                })
                .map_err(|e| change_message(&e))?;
            found = list
                .drafts
                .into_iter()
                .find(|d| d.message.as_ref().is_some_and(|m| m.thread_id == thread_id));
            page = list.next_page_token;
            if found.is_some() || page.is_none() {
                break;
            }
        }
        let draft = found.ok_or("This draft is no longer in Gmail.")?;
        let message_id = draft
            .message
            .as_ref()
            .map_or("", |m| m.id.as_str())
            .to_string();
        let message_id = message_id.as_str();
        let dto = self
            .with_token(account, |agent, ep, token| {
                gmail::message_full(agent, ep, token, message_id)
            })
            .map_err(|e| change_message(&e))?;
        let m = parse::parse_message(&dto);
        if !m.attachments.is_empty() {
            return Err("Drafts with attachments can't be edited here yet.".into());
        }
        let reply = m.in_reply_to.is_some();
        let from = if m.from.email.is_empty() {
            Address::new(None, account)
        } else {
            m.from
        };
        Ok((
            draft.id,
            Box::new(Outgoing {
                account: account.to_string(),
                kind: if reply {
                    ComposeKind::Reply
                } else {
                    ComposeKind::New
                },
                from,
                to: m.to,
                cc: m.cc,
                bcc: m.bcc,
                subject: m.subject,
                body: m.body,
                thread_id: Some(m.thread_id).filter(|t| !t.is_empty()),
                in_reply_to: m.in_reply_to,
                references: m.references,
                attachments: Vec::new(),
            }),
        ))
    }

    /// SPEC 7.5 `SaveAttachment`: download the attachment afresh, then write it to a new file
    /// in the download folder under a sanitised name. The outcome is a toast.
    pub(super) fn save(&mut self, d: Download) {
        let name = files::sanitise(&d.attachment.filename);
        let data = match self.attachment_base64(&d.account, &d.message_id, &d.attachment.part_id) {
            Ok(data) => {
                self.succeeded(&d.account);
                data
            }
            Err(e) => {
                let message = attachment_message(&e);
                self.failed(&d.account, &e);
                self.emit(SyncEvent::Notice(format!(
                    "Couldn't save {name}: {message}"
                )));
                return;
            }
        };
        // Bad data in one attachment is that attachment's problem, not the account's: a
        // toast, with no backoff or banner.
        let Ok(bytes) = b64::decode(&data) else {
            self.emit(SyncEvent::Notice(format!(
                "Couldn't save {name}: {ATTACHMENT_UNREADABLE}"
            )));
            return;
        };
        let saved = match &self.downloads {
            Some(dir) => std::fs::create_dir_all(dir).map(|()| dir.clone()),
            None => files::download_dir(),
        }
        .and_then(|dir| files::save_new(&dir, &name, &bytes));
        match saved {
            Ok(path) => self.emit(SyncEvent::Saved(path)),
            Err(e) => {
                eprintln!("eloquawk: couldn't save an attachment: {}", e.kind());
                self.emit(SyncEvent::Notice(format!(
                    "Couldn't save {name}: {}",
                    io_message(&e)
                )));
            }
        }
    }

    /// The base64url data of part `part_id` of message `message_id`, still encoded: decoding
    /// is the caller's, since bad data is no reason to back the account off. The message is
    /// read first for a fresh attachment id, since Gmail's are not stable; a part whose data
    /// is inline (no attachment id) needs no second call. A part that is gone is `NotFound`.
    pub(super) fn attachment_base64(
        &mut self,
        email: &str,
        message_id: &str,
        part_id: &str,
    ) -> Result<String, CallError> {
        let message = self.with_token(email, |agent, ep, token| {
            gmail::message_full(agent, ep, token, message_id)
        })?;
        let body = message
            .payload
            .as_ref()
            .and_then(|root| find_part(root, part_id))
            .and_then(|part| part.body.clone())
            .ok_or(CallError::Api(ApiError::NotFound))?;
        let data = match (body.attachment_id, body.data) {
            (Some(attachment_id), _) => {
                self.with_token(email, |agent, ep, token| {
                    gmail::attachment(agent, ep, token, message_id, &attachment_id, body.size)
                })?
                .data
            }
            (None, Some(data)) => data,
            (None, None) => return Err(CallError::Api(ApiError::NotFound)),
        };
        Ok(data)
    }

    pub(super) fn deliver(&mut self, out: &Outgoing) -> Result<(), SendError> {
        let parts = gather_attachments(out, |message_id, part_id| {
            let data = self
                .attachment_base64(&out.account, message_id, part_id)
                .map_err(|e| attachment_message(&e))?;
            b64::decode(&data).map_err(|_| ATTACHMENT_UNREADABLE.to_string())
        })?;
        let message_id = compose::new_message_id();
        let raw =
            compose::build(out, &parts, Timestamp::now(), &message_id).map_err(|e| SendError {
                message: e.to_string(),
                maybe_sent: false,
            })?;
        drop(parts);
        let thread_id = out.thread_id.as_deref();
        self.with_token(&out.account, |agent, ep, token| {
            gmail::send(agent, ep, token, &raw, thread_id)
        })
        .map(drop)
        .map_err(|e| send_error(&e))
    }
}

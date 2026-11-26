use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::model::Address;

const ME: &str = "you@example.com";
const WORK: &str = "you@work.example";

fn store() -> (Store, mpsc::Receiver<Command>) {
    let (tx, rx) = mpsc::channel();
    (Store::for_tests(SyncHandle::from_sender(tx)), rx)
}

fn view(label: &str) -> ViewKey {
    ViewKey {
        account: ME.into(),
        kind: ViewKind::Label(label.into()),
    }
}

fn all_mail() -> ViewKey {
    ViewKey {
        account: ME.into(),
        kind: ViewKind::AllMail,
    }
}

fn key(id: &str) -> ThreadKey {
    ThreadKey::new(ME, id)
}

/// Thread `t{n}`, `n` minutes old, so lower numbers sort first.
fn summary(n: usize, labels: &[&str]) -> ThreadSummary {
    ThreadSummary {
        key: key(&format!("t{n}")),
        history_id: 1,
        subject: format!("Subject {n}"),
        snippet: String::new(),
        participants: vec![(Address::new(Some("Ann"), "ann@example.org"), false)],
        last_date: Timestamp::from_second(1_800_000_000 - 60 * n as i64).unwrap(),
        message_count: 1,
        label_ids: labels.iter().map(|s| s.to_string()).collect(),
        has_attachments: false,
        last: None,
    }
}

fn label(id: &str, kind: LabelKind, unread: u32, total: u32) -> Label {
    Label {
        account: ME.into(),
        id: id.into(),
        name: id.into(),
        kind,
        color: None,
        threads_unread: unread,
        threads_total: total,
        messages_total: total,
        hidden: false,
    }
}

fn ids(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|n| format!("t{n}")).collect()
}

fn page(
    s: &mut Store,
    v: &ViewKey,
    range: std::ops::Range<usize>,
    labels: &[&str],
    mode: PageMode,
    has_more: bool,
) {
    s.apply(SyncEvent::ThreadPage {
        view: v.clone(),
        ids: ids(range.clone()),
        threads: range.map(|n| summary(n, labels)).collect(),
        mode,
        has_more,
    });
}

/// A store showing INBOX with `n` threads (t0..tn) that are also in All Mail.
fn inbox_store(n: usize) -> (Store, mpsc::Receiver<Command>) {
    let (mut s, rx) = store();
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels: vec![
            label("INBOX", LabelKind::System, 0, n as u32),
            label("SPAM", LabelKind::System, 0, 0),
            label("TRASH", LabelKind::System, 0, 0),
        ],
    });
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    page(
        &mut s,
        &all_mail(),
        0..n,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    page(
        &mut s,
        &view("INBOX"),
        0..n,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    drain(&rx);
    (s, rx)
}

fn drain(rx: &mpsc::Receiver<Command>) -> Vec<Command> {
    rx.try_iter().collect()
}

fn list(s: &Store, v: &ViewKey) -> Vec<String> {
    s.lists.get(v).map(|l| l.ids.clone()).unwrap_or_default()
}

fn label_count(s: &Store, id: &str) -> (u32, u32) {
    let l = s.labels[ME].iter().find(|l| l.id == id).unwrap();
    (l.threads_unread, l.threads_total)
}

#[test]
fn apply_every_event() {
    let (mut s, rx) = store();
    s.apply(SyncEvent::Auth(AuthState::Demo));
    assert_eq!(s.auth, AuthState::Demo);
    s.apply(SyncEvent::Client(Some("id".into())));
    assert_eq!(s.client_id.as_deref(), Some("id"));
    s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
    assert_eq!(s.accounts.len(), 2);
    assert_eq!(s.view, Some(view("INBOX")));
    assert!(matches!(
        drain(&rx).as_slice(),
        [Command::Want { more: false, .. }]
    ));
    s.apply(SyncEvent::SigningIn(Some(
        "https://accounts.example".into(),
    )));
    assert!(s.signing_in.is_some());
    s.apply(SyncEvent::Busy(true));
    assert!(s.busy);
    s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
    assert_eq!(s.last_sync, Some(Timestamp::UNIX_EPOCH));
    s.apply(SyncEvent::Problem(Some("Offline".into())));
    assert_eq!(s.problem.as_deref(), Some("Offline"));
    s.apply(SyncEvent::Notice("Hello".into()));
    assert_eq!(s.notice.as_ref().map(|n| n.text.as_str()), Some("Hello"));
    let saved = std::path::PathBuf::from("/srv/dl/a (1).pdf");
    s.apply(SyncEvent::Saved(saved.clone()));
    let notice = s.notice.clone().unwrap();
    assert_eq!(notice.text, "Saved a (1).pdf to /srv/dl");
    assert_eq!(notice.action, Some(NoticeAction::OpenFolder(saved)));
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels: vec![
            label("INBOX", LabelKind::System, 3, 10),
            label("Label_1", LabelKind::User, 1, 2),
        ],
    });
    assert_eq!(s.unread_badge(ME, "INBOX"), Some(3));
    s.apply(SyncEvent::LabelCounts {
        account: ME.into(),
        labels: vec![label("INBOX", LabelKind::System, 5, 11)],
    });
    assert_eq!(s.unread_badge(ME, "INBOX"), Some(5));
    assert_eq!(s.labels[ME].len(), 2);
    page(
        &mut s,
        &view("INBOX"),
        0..3,
        &["INBOX", "UNREAD"],
        PageMode::Replace,
        true,
    );
    assert_eq!(list(&s, &view("INBOX")), ids(0..3));
    assert_eq!(s.cursor, Some(key("t0")));
    assert_eq!(s.visible_rows().len(), 3);
    let mut updated = summary(1, &["INBOX"]);
    updated.subject = "Changed".into();
    s.apply(SyncEvent::ThreadsUpdated(vec![updated]));
    assert_eq!(s.threads[&key("t1")].subject, "Changed");
    let detail = ThreadDetail {
        key: key("t0"),
        subject: "Subject 0".into(),
        label_ids: BTreeSet::new(),
        messages: vec![],
    };
    s.apply(SyncEvent::ThreadDetail(detail));
    assert!(matches!(
        s.details.get(&key("t0")),
        Some(DetailState::Loaded(_))
    ));
    s.apply(SyncEvent::ThreadFailed {
        key: key("t1"),
        error: "Gone".into(),
    });
    assert_eq!(
        s.details.get(&key("t1")),
        Some(&DetailState::Failed("Gone".into()))
    );
    s.apply(SyncEvent::MutationFinished {
        op: 99,
        error: None,
    });
    s.apply(SyncEvent::SendFinished { op: 7, error: None });
    assert_eq!(s.send_state(7), Some(&SendState::Sent));
    assert_eq!(
        s.notice.as_ref().map(|n| n.text.as_str()),
        Some("Message sent")
    );
    let failure = SendError {
        message: "No".into(),
        maybe_sent: true,
    };
    s.apply(SyncEvent::SendFinished {
        op: 8,
        error: Some(failure.clone()),
    });
    assert_eq!(s.send_state(8), Some(&SendState::Failed(failure)));
    s.forget_send(8);
    assert_eq!(s.send_state(8), None);
    s.apply(SyncEvent::NewMail {
        account: ME.into(),
        threads: vec![summary(9, &["INBOX", "UNREAD"])],
    });
    assert_eq!(s.take_new_mail().len(), 1);
    s.apply(SyncEvent::ThreadsGone {
        account: ME.into(),
        ids: vec!["t0".into()],
    });
    assert_eq!(list(&s, &view("INBOX")), ids(1..3));
    assert!(!s.threads.contains_key(&key("t0")));
    assert_eq!(s.cursor, Some(key("t1")));
    s.apply(SyncEvent::AccountReset(ME.into()));
    assert!(s.threads.keys().all(|k| k.account != ME));
    assert!(s.lists.keys().all(|v| v.account != ME));
    assert!(s.details.keys().all(|k| k.account != ME));
}

#[test]
fn cursor_moves_and_requests_more() {
    let (mut s, rx) = store();
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    drain(&rx);
    page(
        &mut s,
        &view("INBOX"),
        0..50,
        &["INBOX"],
        PageMode::Replace,
        true,
    );
    assert_eq!(s.cursor_index(), Some(0));
    s.move_cursor(-1);
    assert_eq!(s.cursor_index(), Some(0));
    s.move_cursor(1);
    assert_eq!(s.cursor_index(), Some(1));
    s.move_cursor(38);
    assert_eq!(s.cursor_index(), Some(39));
    assert!(drain(&rx).is_empty(), "row 39 of 50 is not in the last ten");
    s.move_cursor(1);
    assert_eq!(s.cursor_index(), Some(40));
    let cmds = drain(&rx);
    assert!(matches!(
        cmds.as_slice(),
        [Command::Want { more: true, .. }]
    ));
    s.move_cursor(1);
    assert!(
        drain(&rx).is_empty(),
        "the request is not repeated while loading"
    );
    s.move_cursor(100);
    assert_eq!(s.cursor_index(), Some(49));
    page(
        &mut s,
        &view("INBOX"),
        50..60,
        &["INBOX"],
        PageMode::Append,
        false,
    );
    assert_eq!(s.cursor_index(), Some(49));
    s.move_cursor(5);
    assert_eq!(s.cursor_index(), Some(54));
    assert!(drain(&rx).is_empty(), "no more pages");
}

#[test]
fn archive_removes_from_inbox_and_auto_advances() {
    let (mut s, rx) = inbox_store(5);
    s.move_cursor(2);
    s.open_cursor();
    assert_eq!(s.open, Some(key("t2")));
    drain(&rx);
    let op = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t1", "t3", "t4"]);
    assert_eq!(list(&s, &all_mail()), ids(0..5), "archive keeps All Mail");
    assert!(!s.threads[&key("t2")].in_label("INBOX"));
    assert_eq!(label_count(&s, "INBOX").1, 4);
    assert_eq!(s.open, Some(key("t3")));
    assert_eq!(s.cursor, Some(key("t3")));
    let cmds = drain(&rx);
    assert!(cmds.iter().any(|c| matches!(c,
        Command::Modify { op: o, thread_ids, delta, .. }
            if *o == op && thread_ids == &["t2"] && *delta == LabelDelta::archive())));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::OpenThread(k) if *k == key("t3")))
    );
    let notice = s.notice.clone().unwrap();
    assert_eq!(notice.text, "Archived 1 conversation");
    assert_eq!(notice.action, Some(NoticeAction::Undo));
    s.apply(SyncEvent::MutationFinished { op, error: None });
    assert!(!s.is_pending(&key("t2")));

    // The last row falls back to the previous one, then the reader closes.
    s.set_cursor_index(3);
    s.open_cursor();
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    assert_eq!(s.open, Some(key("t3")));
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    assert!(list(&s, &view("INBOX")).is_empty());
    assert_eq!(s.open, None);
    assert_eq!(s.cursor, None);
}

#[test]
fn rollback_restores_index() {
    let (mut s, rx) = inbox_store(5);
    s.set_cursor_index(2);
    let op = s.mutate(MutationKind::Trash).unwrap();
    assert!(matches!(drain(&rx).as_slice(), [Command::Trash { .. }]));
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t1", "t3", "t4"]);
    assert_eq!(list(&s, &all_mail()), ["t0", "t1", "t3", "t4"]);
    assert_eq!(s.cursor, Some(key("t3")));
    assert_eq!(label_count(&s, "TRASH").1, 1);
    s.apply(SyncEvent::MutationFinished {
        op,
        error: Some("server said no".into()),
    });
    assert_eq!(list(&s, &view("INBOX")), ids(0..5));
    assert_eq!(list(&s, &all_mail()), ids(0..5));
    assert!(s.threads[&key("t2")].in_label("INBOX"));
    assert!(!s.threads[&key("t2")].in_label("TRASH"));
    assert_eq!(label_count(&s, "TRASH").1, 0);
    assert_eq!(label_count(&s, "INBOX").1, 5);
    let notice = s.notice.clone().unwrap();
    assert_eq!(notice.text, "Couldn't move to Trash: server said no");
    assert_eq!(notice.action, None);
    assert!(!s.undo(), "a rolled-back op cannot be undone");
}

#[test]
fn undo_goes_with_its_toast() {
    let (mut s, rx) = inbox_store(4);
    s.set_cursor_index(1);
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    s.apply(SyncEvent::Notice("Synced".into()));
    assert!(
        !s.undo(),
        "a notice without Undo takes the undo slot with it"
    );

    // A different op's failure toast does the same.
    s.set_cursor_index(0);
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    s.set_cursor_index(0);
    let star = s.mutate(MutationKind::Labels(LabelDelta::star())).unwrap();
    s.apply(SyncEvent::MutationFinished {
        op: star,
        error: Some("nope".into()),
    });
    assert!(!s.undo());
    drain(&rx);
}

#[test]
fn rollback_keeps_other_pending_ops() {
    let (mut s, rx) = inbox_store(4);
    s.set_cursor_index(1);
    let archive = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    // Starred after the archive's snapshot was taken, still pending when it fails.
    let star = s
        .mutate_keys(
            vec![key("t1")],
            MutationKind::Labels(LabelDelta::star()),
            false,
            None,
        )
        .unwrap();
    drain(&rx);
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
    s.apply(SyncEvent::MutationFinished {
        op: archive,
        error: Some("server said no".into()),
    });
    let t1 = &s.threads[&key("t1")];
    assert!(t1.in_label("INBOX"));
    assert!(t1.starred(), "the still-pending star survives the rollback");
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    assert_eq!(label_count(&s, "INBOX").1, 4);
    s.apply(SyncEvent::MutationFinished {
        op: star,
        error: None,
    });
    assert!(s.threads[&key("t1")].starred());
}

#[test]
fn pending_join_survives_stale_page() {
    let (mut s, rx) = inbox_store(4);
    s.set_cursor_index(1);
    let archive = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    s.apply(SyncEvent::MutationFinished {
        op: archive,
        error: None,
    });
    assert!(s.undo());
    drain(&rx);
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    // Listed before the worker applied the undo: t1 is missing.
    s.apply(SyncEvent::ThreadPage {
        view: view("INBOX"),
        ids: vec!["t0".into(), "t2".into(), "t3".into()],
        threads: vec![],
        mode: PageMode::Replace,
        has_more: false,
    });
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    s.apply(SyncEvent::ThreadPage {
        view: view("INBOX"),
        ids: vec!["t0".into(), "t2".into(), "t3".into()],
        threads: vec![],
        mode: PageMode::Refresh,
        has_more: false,
    });
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
}

#[test]
fn grouped_untrash_restores_order_and_rolls_back_alone() {
    let (mut s, rx) = inbox_store(4);
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(
        0,
        &["INBOX", "Label_1"],
    )]));
    s.toggle_select(key("t0"));
    s.toggle_select(key("t2"));
    s.mutate(MutationKind::Trash);
    assert_eq!(list(&s, &view("INBOX")), ["t1", "t3"]);
    drain(&rx);
    assert!(s.undo());
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    assert_eq!(list(&s, &all_mail()), ids(0..4));
    let t0_op = drain(&rx)
        .into_iter()
        .find_map(|c| match c {
            Command::Untrash { op, thread_ids, .. } if thread_ids == ["t0"] => Some(op),
            _ => None,
        })
        .unwrap();
    s.apply(SyncEvent::MutationFinished {
        op: t0_op,
        error: Some("server said no".into()),
    });
    assert_eq!(list(&s, &view("INBOX")), ["t1", "t2", "t3"]);
    assert_eq!(list(&s, &all_mail()), ["t1", "t2", "t3"]);
    assert!(s.threads[&key("t0")].in_label("TRASH"));
    assert!(!s.threads[&key("t2")].in_label("TRASH"));
}

#[test]
fn pending_delta_reapplied_over_fresh_data() {
    let (mut s, rx) = inbox_store(4);
    s.set_cursor_index(1);
    let op = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    drain(&rx);
    // Stale data from before the change arrives while the op is pending.
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(
        1,
        &["INBOX", "UNREAD"],
    )]));
    assert!(!s.threads[&key("t1")].in_label("INBOX"));
    assert!(
        s.threads[&key("t1")].unread(),
        "the rest of the fresh data is kept"
    );
    page(
        &mut s,
        &view("INBOX"),
        0..4,
        &["INBOX"],
        PageMode::Refresh,
        false,
    );
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
    page(
        &mut s,
        &view("INBOX"),
        0..4,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
    s.apply(SyncEvent::MutationFinished { op, error: None });
    // Authoritative data after the op is taken as it is.
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(1, &[])]));
    assert!(s.threads[&key("t1")].label_ids.is_empty());
    page(
        &mut s,
        &view("INBOX"),
        0..4,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
}

#[test]
fn undo_applies_inverse() {
    let (mut s, rx) = inbox_store(4);
    s.set_cursor_index(1);
    s.open_cursor();
    drain(&rx);
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    assert_eq!(s.open, Some(key("t2")));
    drain(&rx);
    assert!(s.undo());
    let cmds = drain(&rx);
    assert!(matches!(cmds.as_slice(),
        [Command::Modify { thread_ids, delta, .. }]
            if thread_ids == &["t1"] && *delta == LabelDelta::archive().inverse()));
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    assert!(s.threads[&key("t1")].in_label("INBOX"));
    assert_eq!(label_count(&s, "INBOX").1, 4);
    assert_eq!(s.notice.as_ref().unwrap().text, "Action undone");
    assert!(!s.undo(), "undo is single-shot");

    // Read and star changes are not undoable.
    s.mutate(MutationKind::Labels(LabelDelta::star()));
    assert!(!s.undo());
}

#[test]
fn undo_trash_untrashes_and_restores_labels() {
    let (mut s, rx) = inbox_store(3);
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(
        0,
        &["INBOX", "Label_1"],
    )]));
    page(
        &mut s,
        &view("TRASH"),
        5..6,
        &["TRASH"],
        PageMode::Replace,
        false,
    );
    s.set_view(view("INBOX"));
    page(
        &mut s,
        &view("INBOX"),
        0..3,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(
        0,
        &["INBOX", "Label_1"],
    )]));
    drain(&rx);
    s.toggle_select(key("t0"));
    s.toggle_select(key("t1"));
    s.mutate(MutationKind::Trash);
    assert_eq!(list(&s, &view("INBOX")), ["t2"]);
    assert_eq!(list(&s, &view("TRASH")), ["t0", "t1", "t5"]);
    drain(&rx);
    assert!(s.undo());
    let mut restores: Vec<(Vec<String>, Vec<String>)> = drain(&rx)
        .into_iter()
        .filter_map(|c| match c {
            Command::Untrash {
                thread_ids,
                restore,
                ..
            } => Some((thread_ids, restore.add)),
            _ => None,
        })
        .collect();
    restores.sort();
    assert_eq!(
        restores,
        vec![
            (
                vec!["t0".to_string()],
                vec!["INBOX".to_string(), "Label_1".to_string()]
            ),
            (vec!["t1".to_string()], vec!["INBOX".to_string()]),
        ]
    );
    assert_eq!(list(&s, &view("INBOX")), ids(0..3));
    assert_eq!(list(&s, &view("TRASH")), ["t5"]);
    assert!(!s.threads[&key("t0")].in_label("TRASH"));
    assert!(s.threads[&key("t0")].in_label("Label_1"));
}

#[test]
fn spam_removes_from_label_views_and_all_mail() {
    let (mut s, rx) = inbox_store(3);
    let search = ViewKey {
        account: ME.into(),
        kind: ViewKind::Search("ann".into()),
    };
    page(&mut s, &search, 0..3, &["INBOX"], PageMode::Replace, false);
    let anywhere = ViewKey {
        account: ME.into(),
        kind: ViewKind::Search("ann in:anywhere".into()),
    };
    page(
        &mut s,
        &anywhere,
        0..3,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    page(
        &mut s,
        &view("Label_1"),
        0..3,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    page(
        &mut s,
        &view("SPAM"),
        7..8,
        &["SPAM"],
        PageMode::Replace,
        false,
    );
    s.set_view(view("INBOX"));
    drain(&rx);
    s.set_cursor_index(0);
    s.mutate(MutationKind::Labels(LabelDelta::spam()));
    assert_eq!(list(&s, &view("INBOX")), ["t1", "t2"]);
    assert_eq!(list(&s, &view("Label_1")), ["t1", "t2"]);
    assert_eq!(list(&s, &all_mail()), ["t1", "t2"]);
    assert_eq!(list(&s, &search), ["t1", "t2"], "search leaves out Spam");
    assert_eq!(list(&s, &anywhere), ids(0..3), "unless it asks for Spam");
    assert_eq!(list(&s, &view("SPAM")), ["t0", "t7"], "joins Spam by date");
    assert_eq!(
        s.notice.as_ref().unwrap().text,
        "Marked 1 conversation as spam"
    );
    assert!(s.undo());
    assert_eq!(list(&s, &search), ids(0..3), "undo puts the result back");
}

#[test]
fn refresh_page_keeps_tail_and_cursor() {
    let (mut s, rx) = store();
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    drain(&rx);
    let inbox = view("INBOX");
    page(&mut s, &inbox, 0..5, &["INBOX"], PageMode::Replace, true);
    page(&mut s, &inbox, 5..10, &["INBOX"], PageMode::Append, false);
    s.set_cursor_index(7);
    // A new thread arrives at the top and t2 was archived elsewhere.
    let mut fresh = vec!["new".to_string()];
    fresh.extend(["t0", "t1", "t3", "t4"].map(String::from));
    s.apply(SyncEvent::ThreadPage {
        view: inbox.clone(),
        ids: fresh.clone(),
        threads: vec![],
        mode: PageMode::Refresh,
        has_more: true,
    });
    let mut want = fresh.clone();
    want.extend(ids(5..10));
    assert_eq!(list(&s, &inbox), want);
    assert_eq!(s.cursor, Some(key("t7")), "the cursor follows its key");
    assert!(
        !s.lists[&inbox].has_more,
        "has_more only changes for a single page"
    );
    assert!(matches!(s.visible_rows()[0], Row::Skeleton(_)));

    // The cursor's own row disappears: it moves to the row at its old index.
    s.set_cursor_index(1);
    s.apply(SyncEvent::ThreadPage {
        view: inbox.clone(),
        ids: ["new", "t1", "t3", "t4"].map(String::from).to_vec(),
        threads: vec![],
        mode: PageMode::Refresh,
        has_more: true,
    });
    assert_eq!(s.cursor, Some(key("t1")));

    // A page whose last id is unknown replaces that many rows and keeps the rest.
    assert_eq!(
        merge_refresh(&ids(0..5), &["x".to_string()]),
        ["x", "t1", "t2", "t3", "t4"]
    );
    assert_eq!(
        merge_refresh(
            &ids(0..5),
            &["x".to_string(), "t0".to_string(), "y".to_string()]
        ),
        ["x", "t0", "y", "t3", "t4"]
    );
    assert!(merge_refresh(&ids(0..5), &[]).is_empty());
}

#[test]
fn append_dedupes() {
    let (mut s, rx) = store();
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    drain(&rx);
    let inbox = view("INBOX");
    page(&mut s, &inbox, 0..5, &["INBOX"], PageMode::Replace, true);
    page(&mut s, &inbox, 3..8, &["INBOX"], PageMode::Append, true);
    assert_eq!(list(&s, &inbox), ids(0..8));
    s.apply(SyncEvent::ThreadPage {
        view: inbox.clone(),
        ids: ["t8", "t8", "t0"].map(String::from).to_vec(),
        threads: vec![],
        mode: PageMode::Append,
        has_more: false,
    });
    assert_eq!(list(&s, &inbox), ids(0..9));
    assert!(!s.lists[&inbox].has_more);
}

#[test]
fn new_mail_only_when_pref_on() {
    let (mut s, _rx) = store();
    let ev = || SyncEvent::NewMail {
        account: ME.into(),
        threads: vec![summary(0, &["INBOX", "UNREAD"])],
    };
    s.apply(ev());
    assert_eq!(s.take_new_mail().len(), 1);
    assert!(s.take_new_mail().is_empty());
    s.prefs.notifications = false;
    s.apply(ev());
    assert!(s.take_new_mail().is_empty());
}

#[test]
fn new_mail_keeps_only_the_named_accounts_summaries() {
    let (mut s, _rx) = store();
    let mut theirs = summary(1, &["INBOX", "UNREAD"]);
    theirs.key.account = "someone@example.org".into();
    s.apply(SyncEvent::NewMail {
        account: ME.into(),
        threads: vec![theirs.clone()],
    });
    assert!(s.take_new_mail().is_empty(), "another account's summary");
    s.apply(SyncEvent::NewMail {
        account: ME.into(),
        threads: vec![theirs, summary(0, &["INBOX", "UNREAD"])],
    });
    let kept = s.take_new_mail();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].key.account, ME);
}

#[test]
fn settings_changes_prefs_in_range_and_stays_ephemeral() {
    let (mut s, rx) = store();
    s.change_prefs(|p| {
        p.clock_24h = false;
        p.refresh_secs = 5;
    });
    assert!(!s.prefs.clock_24h);
    assert_eq!(s.prefs.refresh_secs, crate::config::REFRESH_SECS_MIN);
    // The worker hears about the new interval, clamped.
    assert!(matches!(
        drain(&rx).as_slice(),
        [Command::Tune { refresh_secs, .. }] if *refresh_secs == crate::config::REFRESH_SECS_MIN
    ));
    s.change_prefs(|p| p.clock_24h = true);
    assert!(
        drain(&rx).is_empty(),
        "other prefs do not retune the worker"
    );
    // Ephemeral prefs (demo, tests) stay ephemeral, so `save` can never write them.
    assert!(s.prefs.ephemeral);
}

#[test]
fn abandoned_send_failure_becomes_a_notice() {
    let (mut s, _rx) = store();
    let op = s.send(crate::ui::compose::blank(&ME.to_string()));
    assert_eq!(s.send_state(op), Some(&SendState::Sending));
    s.abandon_send(op);
    assert_eq!(s.send_state(op), None);
    s.apply(SyncEvent::SendFinished {
        op,
        error: Some(SendError {
            message: "No".into(),
            maybe_sent: false,
        }),
    });
    assert_eq!(s.send_state(op), None, "nothing is left behind");
    assert_eq!(
        s.notice.as_ref().map(|n| n.text.as_str()),
        Some("Couldn't send the message")
    );
    // A settled send that is abandoned is just forgotten.
    let op = s.send(crate::ui::compose::blank(&ME.to_string()));
    s.apply(SyncEvent::SendFinished { op, error: None });
    s.abandon_send(op);
    assert_eq!(s.send_state(op), None);
}

fn sent(rx: &mpsc::Receiver<Command>) -> usize {
    drain(rx)
        .iter()
        .filter(|c| matches!(c, Command::Send { .. }))
        .count()
}

#[test]
fn undoable_send_is_held_until_its_window_ends() {
    let (mut s, rx) = store();
    let (op, wait) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    assert_eq!(wait, Some(Duration::from_secs(10)));
    assert_eq!(s.send_state(op), Some(&SendState::Scheduled));
    assert_eq!(sent(&rx), 0, "nothing reaches the worker yet");
    assert!(s.sends_unsettled());
    s.dispatch_scheduled(op);
    assert_eq!(s.send_state(op), Some(&SendState::Sending));
    assert_eq!(sent(&rx), 1);
    // A second timer firing for the same op sends nothing more.
    s.dispatch_scheduled(op);
    assert_eq!(sent(&rx), 0);
}

#[test]
fn cancelling_returns_the_message_and_nothing_leaves() {
    let (mut s, rx) = store();
    let mut out = crate::ui::compose::blank(&ME.to_string());
    out.subject = "Hello".into();
    let (op, _) = s.send_undoable(out);
    let back = s.cancel_send(op).expect("still held");
    assert_eq!(back.subject, "Hello");
    assert_eq!(s.send_state(op), None);
    assert!(!s.sends_unsettled());
    // The timer fires afterwards: nothing to send.
    s.dispatch_scheduled(op);
    assert_eq!(sent(&rx), 0);
    // Too late to undo once it left.
    let (op, _) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.dispatch_scheduled(op);
    assert!(s.cancel_send(op).is_none());
    assert_eq!(s.send_state(op), Some(&SendState::Sending));
}

#[test]
fn zero_delay_sends_at_once_and_flush_ends_every_window() {
    let (mut s, rx) = store();
    s.prefs.undo_send_secs = 0;
    let (op, wait) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    assert_eq!((wait, s.send_state(op)), (None, Some(&SendState::Sending)));
    assert_eq!(sent(&rx), 1);
    s.prefs.undo_send_secs = 10;
    s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.flush_sends();
    assert_eq!(sent(&rx), 2);
}

#[test]
fn a_window_closed_during_the_undo_window_still_reports_failure() {
    let (mut s, rx) = store();
    let (op, _) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.abandon_send(op);
    assert!(s.sends_unsettled(), "held messages block a quit");
    s.dispatch_scheduled(op);
    assert_eq!(sent(&rx), 1);
    s.apply(SyncEvent::SendFinished {
        op,
        error: Some(SendError {
            message: "No".into(),
            maybe_sent: false,
        }),
    });
    assert_eq!(
        s.notice.as_ref().map(|n| n.text.as_str()),
        Some("Couldn't send the message")
    );
    assert!(!s.sends_unsettled());
}

#[test]
fn a_sent_message_takes_its_draft_with_it_and_a_failed_one_keeps_it() {
    let (mut s, rx) = store();
    s.prefs.undo_send_secs = 0;
    let deletes = |rx: &mpsc::Receiver<Command>| {
        drain(rx)
            .into_iter()
            .filter_map(|c| match c {
                Command::DeleteDraft { draft_id, .. } => Some(draft_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let (op, _) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.note_send_draft(op, ME, "d1".into());
    s.apply(SyncEvent::SendFinished {
        op,
        error: Some(SendError {
            message: "No".into(),
            maybe_sent: false,
        }),
    });
    assert!(deletes(&rx).is_empty(), "a failed send keeps the draft");
    let (op, _) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.note_send_draft(op, ME, "d1".into());
    s.apply(SyncEvent::SendFinished { op, error: None });
    assert_eq!(deletes(&rx), ["d1"]);
    // Taking a held message back keeps its draft too.
    s.prefs.undo_send_secs = 10;
    let (op, _) = s.send_undoable(crate::ui::compose::blank(&ME.to_string()));
    s.note_send_draft(op, ME, "d2".into());
    s.cancel_send(op);
    s.apply(SyncEvent::SendFinished { op, error: None });
    assert!(deletes(&rx).is_empty());
}

#[test]
fn enter_in_the_drafts_view_edits_the_draft_instead_of_opening_it() {
    let (mut s, rx) = inbox_store(3);
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels: vec![label("DRAFT", LabelKind::System, 0, 3)],
    });
    // Elsewhere it opens the reader.
    s.activate(key("t0"));
    assert_eq!(s.open, Some(key("t0")));
    drain(&rx);
    s.view = Some(view("DRAFT"));
    s.activate(key("t1"));
    let load = drain(&rx)
        .into_iter()
        .find_map(|c| match c {
            Command::LoadDraft { op, thread_id, .. } => Some((op, thread_id)),
            _ => None,
        })
        .expect("the draft is requested");
    assert_eq!(load.1, "t1");
    assert_eq!(s.open, Some(key("t0")), "the reader did not move");
    assert!(s.take_edit_draft().is_none(), "still loading");
    s.apply(SyncEvent::DraftLoaded {
        op: load.0,
        result: Err("gone".into()),
    });
    assert_eq!(s.take_edit_draft().unwrap().unwrap_err(), "gone");
    assert!(s.take_edit_draft().is_none());
}

#[test]
fn account_removal_prunes() {
    let (mut s, rx) = store();
    s.prefs.last_view = Some((WORK.into(), "Label_9".into()));
    s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
    let work_view = ViewKey {
        account: WORK.into(),
        kind: ViewKind::Label("Label_9".into()),
    };
    assert_eq!(s.view, Some(work_view.clone()), "last_view is restored");
    let mut w = summary(1, &["Label_9"]);
    w.key = ThreadKey::new(WORK, "w1");
    s.apply(SyncEvent::ThreadPage {
        view: work_view.clone(),
        ids: vec!["w1".into()],
        threads: vec![w],
        mode: PageMode::Replace,
        has_more: false,
    });
    s.apply(SyncEvent::Labels {
        account: WORK.into(),
        labels: vec![],
    });
    page(
        &mut s,
        &view("INBOX"),
        0..2,
        &["INBOX"],
        PageMode::Replace,
        false,
    );
    s.open_cursor();
    drain(&rx);
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    assert!(s.threads.keys().all(|k| k.account == ME));
    assert!(s.lists.keys().all(|v| v.account == ME));
    assert!(!s.labels.contains_key(WORK));
    assert_eq!(s.open, None);
    assert_eq!(s.view, Some(view("INBOX")), "falls back to the first inbox");
    assert_eq!(s.cursor, Some(key("t0")));
    assert!(matches!(
        drain(&rx).as_slice(),
        [Command::Want { more: false, .. }]
    ));
    s.apply(SyncEvent::Accounts(vec![]));
    assert_eq!(s.view, None);
    assert!(s.threads.is_empty());
}

#[test]
fn selection_targets_bulk_mutation() {
    let (mut s, rx) = inbox_store(5);
    s.set_cursor_index(0);
    s.open_cursor();
    drain(&rx);
    s.toggle_select(key("t1"));
    s.toggle_select(key("t3"));
    s.toggle_select(key("t4"));
    s.toggle_select(key("t4"));
    assert_eq!(s.targets(), [key("t1"), key("t3")]);
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    let cmds = drain(&rx);
    assert!(matches!(cmds.as_slice(),
        [Command::Modify { thread_ids, .. }] if thread_ids == &["t1", "t3"]));
    assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t4"]);
    assert!(s.selected.is_empty(), "archived rows leave the selection");
    assert_eq!(s.open, Some(key("t0")), "the open thread was not a target");
    assert_eq!(s.notice.as_ref().unwrap().text, "Archived 2 conversations");
    // Without a selection the open thread is the target.
    assert_eq!(s.targets(), [key("t0")]);
}

#[test]
fn view_aware_kinds_follow_the_view() {
    let (mut s, rx) = inbox_store(3);
    assert_eq!(
        s.archive_kind(),
        MutationKind::Labels(LabelDelta::archive())
    );
    assert_eq!(s.trash_kind(), MutationKind::Trash);
    assert_eq!(s.spam_kind(), MutationKind::Labels(LabelDelta::spam()));
    s.set_view(view("SPAM"));
    assert_eq!(
        s.archive_kind(),
        MutationKind::Labels(LabelDelta::not_spam())
    );
    assert_eq!(s.spam_kind(), MutationKind::Labels(LabelDelta::not_spam()));
    s.set_view(view("TRASH"));
    assert_eq!(
        s.trash_kind(),
        MutationKind::Untrash {
            restore: LabelDelta {
                add: vec!["INBOX".into()],
                remove: vec![],
            }
        }
    );
    drain(&rx);
}

#[test]
fn star_kind_unstars_only_when_every_target_is_starred() {
    let (mut s, _rx) = inbox_store(3);
    s.set_cursor_index(0);
    assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::star()));
    s.mutate(MutationKind::Labels(LabelDelta::star()));
    assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::unstar()));
    s.toggle_select(key("t0"));
    s.toggle_select(key("t1"));
    assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::star()));
}

#[test]
fn go_to_and_switch_account() {
    let (mut s, rx) = inbox_store(2);
    s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
    drain(&rx);
    s.go_to(ViewKind::Label("STARRED".into()));
    assert_eq!(s.view, Some(view("STARRED")));
    s.go_to(ViewKind::AllMail);
    assert_eq!(s.view, Some(all_mail()));
    let inbox_of = |a: &str| ViewKey {
        account: a.into(),
        kind: ViewKind::Label("INBOX".into()),
    };
    s.switch_account(1);
    assert_eq!(s.view, Some(inbox_of(WORK)));
    s.switch_account(1);
    assert_eq!(s.view, Some(inbox_of(ME)), "wraps round");
    s.switch_account(-1);
    assert_eq!(s.view, Some(inbox_of(WORK)));
    assert!(
        matches!(drain(&rx).last(), Some(Command::Want { view, more: false })
        if *view == inbox_of(WORK))
    );
}

#[test]
fn search_shows_results_and_returns_to_the_label() {
    let (mut s, rx) = inbox_store(3);
    s.go_to(ViewKind::Label("STARRED".into()));
    drain(&rx);
    s.search("  from:zoë ");
    let search = ViewKey {
        account: ME.into(),
        kind: ViewKind::Search("from:zoë".into()),
    };
    assert_eq!(s.view, Some(search.clone()), "trimmed, sent verbatim");
    assert_eq!(s.search_query(), Some("from:zoë"));
    assert!(
        matches!(drain(&rx).last(), Some(Command::Want { view, more: false }) if *view == search)
    );
    assert_eq!(
        s.prefs.last_view,
        Some((ME.into(), "STARRED".into())),
        "searches are not remembered"
    );
    // A second search still returns to the label, not to the first search.
    s.search("ann");
    assert!(s.end_search());
    assert_eq!(s.view, Some(view("STARRED")));
    assert_eq!(s.search_query(), None);
    assert!(!s.end_search(), "nothing to leave");
    // An empty query leaves the search.
    s.search("x");
    s.search("   ");
    assert_eq!(s.view, Some(view("STARRED")));
    // Without a remembered view it falls back to the inbox.
    s.search("x");
    s.search_return = None;
    assert!(s.end_search());
    assert_eq!(s.view, Some(view("INBOX")));
}

#[test]
fn set_label_toggles_and_updates_counts() {
    let (mut s, rx) = inbox_store(3);
    let mut travel = label("Label_2", LabelKind::User, 0, 0);
    travel.name = "Travel".into();
    let mut labels = s.labels[ME].clone();
    labels.push(travel);
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels,
    });
    // t0 is unread.
    s.threads
        .get_mut(&key("t0"))
        .unwrap()
        .label_ids
        .insert("UNREAD".into());
    drain(&rx);
    let op = s.set_label(vec![key("t0")], "Label_2", true).unwrap();
    assert!(s.threads[&key("t0")].in_label("Label_2"));
    assert_eq!(label_count(&s, "Label_2"), (1, 1), "sidebar count follows");
    assert_eq!(s.unread_badge(ME, "Label_2"), Some(1));
    assert_eq!(
        s.notice.as_ref().map(|n| n.text.as_str()),
        Some("Added \u{201c}Travel\u{201d} to 1 conversation")
    );
    assert!(matches!(
        drain(&rx).as_slice(),
        [Command::Modify { op: o, delta, thread_ids, .. }]
            if *o == op && delta.add == ["Label_2"] && delta.remove.is_empty()
                && thread_ids == &["t0"]
    ));
    assert_eq!(list(&s, &view("INBOX")), ids(0..3), "stays in the inbox");
    s.apply(SyncEvent::MutationFinished { op, error: None });
    // Removing it, then undoing the removal.
    s.set_label(vec![key("t0")], "Label_2", false).unwrap();
    assert!(!s.threads[&key("t0")].in_label("Label_2"));
    assert_eq!(label_count(&s, "Label_2"), (0, 0));
    assert_eq!(
        s.notice.as_ref().map(|n| n.text.as_str()),
        Some("Removed \u{201c}Travel\u{201d} from 1 conversation")
    );
    assert!(s.undo());
    assert!(s.threads[&key("t0")].in_label("Label_2"));
    // A failure rolls back.
    let (mut s2, _rx2) = inbox_store(2);
    let op = s2.set_label(vec![key("t1")], "Label_9", true).unwrap();
    s2.apply(SyncEvent::MutationFinished {
        op,
        error: Some("no".into()),
    });
    assert!(!s2.threads[&key("t1")].in_label("Label_9"));
    assert_eq!(
        s2.notice.as_ref().map(|n| n.text.as_str()),
        Some("Couldn't change labels: no")
    );
}

#[test]
fn synced_events_are_counted() {
    let (mut s, _rx) = store();
    s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
    s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
    assert_eq!(s.syncs, 2);
}

#[test]
fn mutate_threads_ignores_the_selection() {
    let (mut s, rx) = inbox_store(4);
    drain(&rx);
    s.toggle_select(key("t1"));
    s.toggle_select(key("t2"));
    // The row star acts on its own thread only.
    s.mutate_threads(vec![key("t3")], MutationKind::Labels(LabelDelta::star()));
    assert!(matches!(drain(&rx).as_slice(),
        [Command::Modify { thread_ids, delta, .. }]
            if thread_ids == &["t3"] && *delta == LabelDelta::star()));
    assert!(s.threads[&key("t3")].starred());
    assert!(!s.threads[&key("t1")].starred());
    assert!(!s.threads[&key("t2")].starred());
    assert_eq!(s.targets(), [key("t1"), key("t2")], "the selection is kept");

    // An undoable kind posts its undo for that thread alone.
    s.mutate_threads(vec![key("t0")], MutationKind::Labels(LabelDelta::archive()));
    drain(&rx);
    assert_eq!(list(&s, &view("INBOX")), ["t1", "t2", "t3"]);
    assert_eq!(s.notice.as_ref().unwrap().action, Some(NoticeAction::Undo));
    assert!(s.undo());
    assert!(matches!(drain(&rx).as_slice(),
        [Command::Modify { thread_ids, .. }] if thread_ids == &["t0"]));
    assert_eq!(list(&s, &view("INBOX")), ids(0..4));
}

#[test]
fn row_key_is_the_thread_shown() {
    let t = summary(7, &["INBOX"]);
    assert_eq!(Row::Thread(&t).key(), &key("t7"));
    assert_eq!(Row::Skeleton(key("t8")).key(), &key("t8"));
}

#[test]
fn mark_read_on_open_is_not_undoable() {
    let (mut s, rx) = inbox_store(2);
    s.apply(SyncEvent::ThreadsUpdated(vec![summary(
        0,
        &["INBOX", "UNREAD"],
    )]));
    s.open_cursor();
    let cmds = drain(&rx);
    assert!(cmds.iter().any(|c| matches!(c, Command::OpenThread(_))));
    assert!(cmds.iter().any(|c| matches!(c,
        Command::Modify { delta, .. } if *delta == LabelDelta::read())));
    assert!(!s.threads[&key("t0")].unread());
    assert!(s.notice.is_none());
    // A fresh cached detail is not fetched again.
    s.apply(SyncEvent::ThreadDetail(ThreadDetail {
        key: key("t0"),
        subject: String::new(),
        label_ids: BTreeSet::new(),
        messages: vec![],
    }));
    s.open_cursor();
    assert!(drain(&rx).is_empty());
}

#[test]
fn labels_for_orders_and_nests() {
    let (mut s, _rx) = store();
    let user = |id: &str, name: &str, unread| {
        let mut l = label(id, LabelKind::User, unread, 1);
        l.name = name.into();
        l
    };
    let mut hidden = user("Label_9", "Hidden", 0);
    hidden.hidden = true;
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels: vec![
            label("TRASH", LabelKind::System, 0, 1),
            label("INBOX", LabelKind::System, 2, 9),
            label("DRAFT", LabelKind::System, 0, 3),
            label("CATEGORY_SOCIAL", LabelKind::System, 4, 4),
            user("Label_3", "Projects/Website", 0),
            user("Label_2", "Projects", 0),
            user("Label_5", "Projects-old", 0),
            user("Label_1", "Receipts", 1),
            user("Label_4", "Orphan/Child", 0),
            hidden,
        ],
    });
    let entries = s.labels_for(ME);
    let names: Vec<(&str, usize, Option<u32>)> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.depth, e.count))
        .collect();
    assert_eq!(
        names,
        [
            ("Inbox", 0, Some(2)),
            ("Nudges", 0, None),
            ("Starred", 0, None),
            ("Important", 0, None),
            ("Sent", 0, None),
            ("Drafts", 0, Some(3)),
            ("All Mail", 0, None),
            ("Spam", 0, None),
            ("Trash", 0, None),
            ("Orphan/Child", 0, None),
            ("Projects", 0, None),
            ("Website", 1, None),
            ("Projects-old", 0, None),
            ("Receipts", 0, Some(1)),
        ]
    );
}

// ----- AI triage (SPEC 16.10, 16.15) ----------------------------------------------------

mod ai {
    use std::sync::mpsc;

    use super::*;
    use crate::ai::jev::{Kind, THRESHOLDS, verdict_from};
    use crate::ai::{AiCommand, AiEvent, AiHandle, KeyState, Outcome, Priority};
    use crate::demo_data::{self, PERSONAL, WORK};

    struct Rig {
        s: Store,
        sync: mpsc::Receiver<Command>,
        ai: mpsc::Receiver<AiCommand>,
    }

    fn prefs(opted: &[&str], background: bool) -> Prefs {
        let mut p = Prefs {
            ephemeral: true,
            ..Prefs::default()
        };
        for a in opted {
            p.ai.accounts.insert(a.to_string(), DISCLOSURE_VERSION);
        }
        p.ai.background = background;
        p
    }

    fn rig(mode: Mode, prefs: Prefs) -> Rig {
        let (tx, sync) = mpsc::channel();
        let (atx, ai) = mpsc::channel();
        let mut s = Store::for_tests_with_ai(
            SyncHandle::from_sender(tx),
            AiHandle::from_sender(atx),
            mode,
            prefs,
        );
        s.apply(SyncEvent::Accounts(vec![PERSONAL.into(), WORK.into()]));
        let r = Rig { s, sync, ai };
        r.drain();
        r
    }

    /// A Google-mode store with a saved key and these accounts opted in.
    fn ready(opted: &[&str], background: bool) -> Rig {
        let mut r = rig(Mode::Google, prefs(opted, background));
        r.s.apply_ai(AiEvent::Key(KeyState::Saved {
            hint: "sk-or-…abcd".into(),
        }));
        r
    }

    impl Rig {
        fn drain(&self) -> (Vec<Command>, Vec<AiCommand>) {
            (self.sync.try_iter().collect(), self.ai.try_iter().collect())
        }

        fn checks(&self) -> Vec<CheckRequest> {
            self.ai
                .try_iter()
                .filter_map(|c| match c {
                    AiCommand::Check(req) => Some(*req),
                    _ => None,
                })
                .collect()
        }

        fn prefetches(&self) -> Vec<ThreadKey> {
            self.sync
                .try_iter()
                .filter_map(|c| match c {
                    Command::Prefetch(k) => Some(k),
                    _ => None,
                })
                .collect()
        }

        /// Opens a demo thread and delivers its detail.
        fn open(&mut self, id: &str) -> ThreadDetail {
            let d = demo(id);
            self.s.open_thread(d.key.clone());
            self.s.apply(SyncEvent::ThreadDetail(d.clone()));
            d
        }

        fn checked(&mut self, d: &ThreadDetail, p: (f32, f32, f32)) {
            let target = self.s.ai_target(d).unwrap().id.clone();
            self.s.apply_ai(AiEvent::Checked {
                thread: d.key.clone(),
                message_id: target,
                outcome: Outcome {
                    verdict: Some(verdict_from(p.0, p.1, p.2, &THRESHOLDS)),
                    labels: None,
                },
            });
        }
    }

    fn demo(id: &str) -> ThreadDetail {
        demo_data::generate(Timestamp::now())
            .threads
            .into_iter()
            .find(|t| t.0.key.id == id)
            .unwrap_or_else(|| panic!("no {id}"))
            .1
    }

    fn new_mail(r: &mut Rig, ids: &[&str]) {
        let threads = ids
            .iter()
            .map(|id| demo_data::summary_of(&demo(id), 1))
            .collect();
        r.s.apply(SyncEvent::NewMail {
            account: PERSONAL.into(),
            threads,
        });
    }

    #[test]
    fn ai_checks_only_when_opted_in_with_key() {
        // No account opted in: no Check, even with a key.
        let mut r = ready(&[], false);
        r.open("p-ai-phish");
        assert!(r.checks().is_empty());
        // Opted in but no key yet.
        let mut r = rig(Mode::Google, prefs(&[PERSONAL], false));
        r.open("p-ai-phish");
        assert!(r.checks().is_empty());
        // Both: one interactive check of the target.
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].priority, Priority::Interactive);
        assert_eq!(checks[0].message_id, d.messages[0].id);
        assert!(checks[0].verdict && checks[0].labels.is_none());
        // Another account's thread is not checked.
        let mut work = ready(&[WORK], false);
        work.open("p-ai-phish");
        assert!(work.checks().is_empty());
    }

    /// A ready store with PERSONAL opted in, its demo labels loaded and `id` open with its
    /// summary listed, so label changes apply optimistically.
    fn labelled(id: &str) -> (Rig, ThreadDetail) {
        let mut r = ready(&[PERSONAL], false);
        r.s.apply(SyncEvent::Labels {
            account: PERSONAL.into(),
            labels: demo_data::label_defs(PERSONAL),
        });
        let d = demo(id);
        r.s.apply(SyncEvent::ThreadsUpdated(vec![demo_data::summary_of(
            &d, 1,
        )]));
        r.drain();
        let d = r.open(id);
        (r, d)
    }

    fn fp(r: &Rig) -> u64 {
        prompt::label_fingerprint(&r.s.labels[PERSONAL])
    }

    fn checked_labels(r: &mut Rig, d: &ThreadDetail, p: (f32, f32, f32), labels: &[(&str, f32)]) {
        let target = r.s.ai_target(d).unwrap().id.clone();
        let fp = fp(r);
        r.s.apply_ai(AiEvent::Checked {
            thread: d.key.clone(),
            message_id: target,
            outcome: Outcome {
                verdict: Some(verdict_from(p.0, p.1, p.2, &THRESHOLDS)),
                labels: Some((
                    Some(fp),
                    labels.iter().map(|(id, p)| (id.to_string(), *p)).collect(),
                )),
            },
        });
    }

    #[test]
    fn guidance_change_rechecks_and_discards_inflight_old_suggestions() {
        let (mut r, detail) = labelled("p-ai-phish");
        let old = r.checks().pop().unwrap();
        let old_fp = old.labels.unwrap().fingerprint;
        let id = r.s.ai_target(&detail).unwrap().id.clone();
        r.s.apply_ai(AiEvent::Checking {
            thread: detail.key.clone(),
            message_id: id.clone(),
        });
        r.s.change_prefs(|p| p.ai.label_guidance = "TTL/M3: retain three months".into());
        assert!(r.checks().is_empty());
        r.s.apply_ai(AiEvent::Checked {
            thread: detail.key.clone(),
            message_id: id,
            outcome: Outcome {
                verdict: Some(verdict_from(0.0, 0.0, 0.0, &THRESHOLDS)),
                labels: Some((Some(old_fp), vec![])),
            },
        });
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        let ask = checks[0].labels.as_ref().unwrap();
        assert_ne!(ask.fingerprint, old_fp);
        assert_eq!(ask.guidance, "TTL/M3: retain three months");
        assert!(r.s.suggestions(&detail.key).is_empty());
    }

    fn names(s: &Store, key: &ThreadKey) -> Vec<String> {
        s.suggestions(key).into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn open_asks_labels_while_suggestions_are_on() {
        let (r, _) = labelled("p-ai-order");
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        let ask = checks[0].labels.as_ref().expect("labels asked on open");
        assert!(checks[0].verdict);
        assert_eq!(ask.fingerprint, fp(&r));
        let names: Vec<&str> = ask.candidates.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Receipts") && names.contains(&"Projects/Eloquawk"));
        // Off: no label names leave.
        let mut p = prefs(&[PERSONAL], false);
        p.ai.suggest_labels = false;
        let mut r = rig(Mode::Google, p);
        r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
        r.s.apply(SyncEvent::Labels {
            account: PERSONAL.into(),
            labels: demo_data::label_defs(PERSONAL),
        });
        r.open("p-ai-order");
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        assert!(checks[0].labels.is_none());
    }

    #[test]
    fn apply_suggestion_is_undoable_and_hides_chip() {
        let (mut r, d) = labelled("p-ai-order");
        r.drain();
        checked_labels(
            &mut r,
            &d,
            (0.03, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.88), (demo_data::TRAVEL, 0.08)],
        );
        assert_eq!(names(&r.s, &d.key), ["Receipts"]);
        assert!(r.s.apply_suggestion(1).is_none(), "no second suggestion");
        let op = r.s.apply_suggestion(0).expect("applied");
        assert!(r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
        assert!(names(&r.s, &d.key).is_empty(), "the chip disappears");
        let notice = r.s.notice.clone().unwrap();
        assert_eq!(
            notice.text,
            "Added \u{201c}Receipts\u{201d} to 1 conversation"
        );
        assert_eq!(notice.action, Some(NoticeAction::Undo));
        let (sync, ai) = r.drain();
        assert!(
            sync.iter()
                .any(|c| matches!(c, Command::Modify { op: o, .. } if *o == op)),
            "the label change reaches the worker"
        );
        assert!(
            !ai.iter().any(|c| matches!(c, AiCommand::Check(_))),
            "no new check"
        );
        // z: the label goes and the chip comes back, still without a request.
        assert!(r.s.undo());
        assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
        assert_eq!(names(&r.s, &d.key), ["Receipts"]);
        assert!(r.checks().is_empty());
        // ✕ hides the row for the session.
        r.s.dismiss_suggestions();
        assert!(names(&r.s, &d.key).is_empty());
        assert!(r.s.apply_suggestion(0).is_none());
    }

    #[test]
    fn background_answer_without_labels_asks_them_for_the_open_thread() {
        let (mut r, d) = labelled("p-ai-order");
        r.drain();
        // The background check was already in flight when the thread opened.
        let target = r.s.ai_target(&d).unwrap().id.clone();
        r.s.apply_ai(AiEvent::Checking {
            thread: d.key.clone(),
            message_id: target,
        });
        r.checked(&d, (0.03, 0.02, 0.05));
        let checks = r.checks();
        assert_eq!(checks.len(), 1, "the labels go out without a reopen");
        assert!(!checks[0].verdict, "labels only");
        assert!(checks[0].labels.is_some());
        // Once labelled, an answer asks nothing more.
        checked_labels(
            &mut r,
            &d,
            (0.03, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.88)],
        );
        assert!(r.checks().is_empty());
    }

    #[test]
    fn label_rename_reasks_labels_only() {
        let (mut r, d) = labelled("p-ai-order");
        let first = r.checks();
        assert!(first[0].verdict);
        checked_labels(
            &mut r,
            &d,
            (0.03, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.88)],
        );
        // Reopening asks nothing; counts arriving change nothing.
        r.s.close_thread();
        r.s.open_thread(d.key.clone());
        assert!(r.checks().is_empty());
        let mut counted = demo_data::label_defs(PERSONAL);
        for l in &mut counted {
            l.threads_total = 3;
        }
        r.s.apply(SyncEvent::LabelCounts {
            account: PERSONAL.into(),
            labels: counted,
        });
        r.s.close_thread();
        r.s.open_thread(d.key.clone());
        assert!(r.checks().is_empty());
        // A rename changes the fingerprint: labels only, no triage questions.
        let mut renamed = demo_data::label_defs(PERSONAL);
        renamed
            .iter_mut()
            .find(|l| l.id == demo_data::RECEIPTS)
            .unwrap()
            .name = "Purchases".into();
        r.s.apply(SyncEvent::Labels {
            account: PERSONAL.into(),
            labels: renamed,
        });
        r.s.close_thread();
        r.s.open_thread(d.key.clone());
        let again = r.checks();
        assert_eq!(again.len(), 1);
        assert!(!again[0].verdict, "labels only");
        let ask = again[0].labels.as_ref().unwrap();
        assert_ne!(
            ask.fingerprint,
            first[0].labels.as_ref().unwrap().fingerprint
        );
        assert!(ask.candidates.iter().any(|c| c.name == "Purchases"));
        // Until the answer lands there is no row, and 1 does nothing (SPEC 16.12.3).
        let target = r.s.ai_target(&d).unwrap().id.clone();
        r.s.apply_ai(AiEvent::Checking {
            thread: d.key.clone(),
            message_id: target.clone(),
        });
        assert!(names(&r.s, &d.key).is_empty());
        assert!(r.s.apply_suggestion(0).is_none());
        assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
        // A failed labels-only re-ask shows no failure and asks again on the next open.
        r.s.apply_ai(AiEvent::Failed {
            thread: d.key.clone(),
            message_id: target.clone(),
            error: "busy".into(),
            retryable: false,
        });
        assert!(r.s.ai_failure(&d.key).is_none());
        r.s.close_thread();
        r.s.open_thread(d.key.clone());
        let again = r.checks();
        assert_eq!(again.len(), 1);
        assert!(!again[0].verdict, "labels only, again");
        let ask = again[0].labels.as_ref().unwrap();
        r.s.apply_ai(AiEvent::Checking {
            thread: d.key.clone(),
            message_id: target.clone(),
        });
        // The labels-only answer keeps the stored verdict.
        r.s.apply_ai(AiEvent::Checked {
            thread: d.key.clone(),
            message_id: target.clone(),
            outcome: Outcome {
                verdict: None,
                labels: Some((Some(ask.fingerprint), vec![])),
            },
        });
        let res = &r.s.ai.results[&(PERSONAL.to_string(), target)];
        assert!(res.verdict.is_some());
        assert_eq!(res.label_fp, Some(ask.fingerprint));
        assert!(names(&r.s, &d.key).is_empty());
    }

    #[test]
    fn required_group_reserves_low_score_suggestion_even_for_risky_message() {
        let (mut r, d) = labelled("p-ai-phish");
        let mut ttl = r.s.labels[PERSONAL][0].clone();
        ttl.id = "ttl_m3".into();
        ttl.name = "TTL/M3".into();
        ttl.kind = LabelKind::User;
        ttl.hidden = false;
        let mut y5 = ttl.clone();
        y5.id = "ttl_y5".into();
        y5.name = "TTL/Y5".into();
        r.s.labels.get_mut(PERSONAL).unwrap().extend([ttl, y5]);
        r.checks();
        r.s.change_prefs(|p| p.ai.required_label_group = "TTL".into());
        let ask = r.checks().pop().unwrap().labels.unwrap();
        let target = r.s.ai_target(&d).unwrap().id.clone();
        r.s.apply_ai(AiEvent::Checked {
            thread: d.key.clone(),
            message_id: target,
            outcome: Outcome {
                verdict: Some(verdict_from(0.97, 0.0, 0.0, &THRESHOLDS)),
                labels: Some((
                    Some(ask.fingerprint),
                    vec![("ttl_m3".into(), 0.2), ("ttl_y5".into(), 0.4)],
                )),
            },
        });
        let suggestions = r.s.suggestions(&d.key);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].id, "ttl_y5");
        assert_eq!(suggestions[0].p, 0.4);
        r.s.threads
            .get_mut(&d.key)
            .unwrap()
            .label_ids
            .insert("ttl_m3".into());
        assert!(r.s.suggestions(&d.key).is_empty());
    }

    #[test]
    fn danger_message_has_no_suggestions() {
        let (mut r, d) = labelled("p-ai-phish");
        checked_labels(
            &mut r,
            &d,
            (0.97, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.95)],
        );
        assert!(r.s.suggestions(&d.key).is_empty());
        assert!(r.s.apply_suggestion(0).is_none());
        assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
        // The probabilities are still kept.
        let target = r.s.ai_target(&d).unwrap().id.clone();
        assert!(
            r.s.ai.results[&(PERSONAL.to_string(), target)]
                .labels
                .is_some()
        );
        // A Warning hides them too, and so does a local caution under a clean verdict.
        let (mut r, d) = labelled("p-ai-order");
        checked_labels(
            &mut r,
            &d,
            (0.6, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.95)],
        );
        assert!(r.s.suggestions(&d.key).is_empty());
        let (mut r, d) = labelled("p-ai-notice");
        checked_labels(
            &mut r,
            &d,
            (0.04, 0.02, 0.05),
            &[(demo_data::RECEIPTS, 0.92)],
        );
        assert!(r.s.local_caution(&d.key).is_some());
        assert!(r.s.suggestions(&d.key).is_empty());
        // Spam level is not risky: suggestions show.
        let (mut r, d) = labelled("p-ai-order");
        checked_labels(
            &mut r,
            &d,
            (0.03, 0.02, 0.9),
            &[(demo_data::RECEIPTS, 0.95)],
        );
        assert_eq!(names(&r.s, &d.key), ["Receipts"]);
    }

    #[test]
    fn ai_disclosure_version_bump_turns_off() {
        let mut p = prefs(&[], false);
        p.ai.accounts
            .insert(PERSONAL.into(), DISCLOSURE_VERSION - 1);
        let (atx, arx) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        let mut s = Store::for_tests_with_ai(
            SyncHandle::from_sender(tx),
            AiHandle::from_sender(atx),
            Mode::Google,
            p,
        );
        let sent: Vec<AiCommand> = arx.try_iter().collect();
        assert!(matches!(&sent[..], [AiCommand::ForgetAccount(a)] if a == PERSONAL));
        assert!(!s.ai_opted_in(PERSONAL));
        s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
        s.apply(SyncEvent::Accounts(vec![PERSONAL.into()]));
        let d = demo("p-ai-phish");
        s.open_thread(d.key.clone());
        s.apply(SyncEvent::ThreadDetail(d));
        assert!(!arx.try_iter().any(|c| matches!(c, AiCommand::Check(_))));
    }

    /// Upgrading from M16: a consent given to the version 1 text no longer counts, the
    /// worker forgets the account, and turning it on again stores the current version.
    #[test]
    fn m16_consent_needs_the_label_disclosure() {
        let mut p = prefs(&[], false);
        p.ai.accounts.insert(PERSONAL.into(), 1);
        let mut r = rig(Mode::Google, p);
        r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
        assert!(!r.s.ai_opted_in(PERSONAL));
        assert_eq!(
            r.s.prefs.ai.accounts.get(PERSONAL),
            Some(&1),
            "shown as changed"
        );
        r.open("p-ai-phish");
        assert!(r.checks().is_empty());
        r.s.set_ai_consent(PERSONAL, true);
        assert_eq!(
            r.s.prefs.ai.accounts.get(PERSONAL),
            Some(&DISCLOSURE_VERSION)
        );
        assert!(r.s.ai_opted_in(PERSONAL));
        assert_eq!(r.checks().len(), 1, "consent checks the open message");
    }

    #[test]
    fn ai_saved_key_checks_message_opened_before_keyring_load() {
        let mut r = rig(Mode::Google, prefs(&[PERSONAL], false));
        r.open("p-ai-phish");
        assert!(r.checks().is_empty());
        r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
        assert_eq!(r.checks().len(), 1);
    }

    #[test]
    fn ai_never_checks_self_sent_or_draft() {
        let mut r = ready(&[PERSONAL], false);
        r.open("p-sent");
        r.open("p-draft");
        assert!(r.checks().is_empty());
        // A thread whose last message is the user's checks the last inbound one.
        let d = r.open("p-cafe");
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        let zoe = d
            .messages
            .iter()
            .rev()
            .find(|m| !m.label_ids.contains(labels::SENT));
        assert_eq!(checks[0].message_id, zoe.unwrap().id);
    }

    #[test]
    fn ai_fast_open_sends_latest_only() {
        let mut r = ready(&[PERSONAL], false);
        let a = r.open("p-ai-phish");
        r.open("p-ai-scam");
        let target = r.s.ai_target(&a).unwrap().id.clone();
        r.s.apply_ai(AiEvent::Checking {
            thread: a.key.clone(),
            message_id: target,
        });
        // Back to A while it is still checking: no second request.
        r.s.open_thread(a.key.clone());
        let checks = r.checks();
        assert_eq!(
            checks
                .iter()
                .map(|c| c.thread.id.as_str())
                .collect::<Vec<_>>(),
            ["p-ai-phish", "p-ai-scam"]
        );
    }

    #[test]
    fn ai_results_dropped_after_opt_out() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        r.checked(&d, (0.97, 0.0, 0.0));
        assert!(r.s.ai_banner(&d.key).is_some());
        r.drain();
        r.s.change_prefs(|p| {
            p.ai.accounts.remove(PERSONAL);
        });
        let (_, ai) = r.drain();
        assert!(
            ai.iter()
                .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == PERSONAL))
        );
        assert!(r.s.ai_banner(&d.key).is_none());
        assert!(r.s.ai.results.is_empty());
        // A late Checked for an opted-out account is dropped.
        r.checked(&d, (0.97, 0.0, 0.0));
        assert!(r.s.ai.results.is_empty());
    }

    #[test]
    fn ai_spam_level_hidden_in_spam_view() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-spam");
        r.checked(&d, (0.0, 0.0, 0.9));
        assert_eq!(r.s.ai_badge(&d.key), Some(Level::Spam));
        assert_eq!(r.s.ai_banner(&d.key).map(|v| v.level), Some(Level::Spam));
        r.s.set_view(ViewKey {
            account: PERSONAL.into(),
            kind: ViewKind::Label(labels::SPAM.into()),
        });
        assert_eq!(r.s.ai_badge(&d.key), None);
        assert!(r.s.ai_banner(&d.key).is_none());
        // Phishing still shows there.
        let p = r.open("p-ai-phish");
        r.checked(&p, (0.97, 0.0, 0.0));
        assert_eq!(r.s.ai_banner(&p.key).map(|v| v.kind), Some(Kind::Phishing));
    }

    #[test]
    fn local_caution_and_guard_without_opt_in() {
        // No consent and no key at all.
        let mut r = rig(Mode::Google, prefs(&[], false));
        let d = r.open("p-ai-notice");
        assert!(r.checks().is_empty());
        let caution = r.s.local_caution(&d.key).unwrap();
        assert_eq!(caution[0], "Sender failed DMARC");
        assert!(caution[1].contains("paypa1-secure.example"));
        let target = d.messages.last().unwrap().id.clone();
        assert!(r.s.link_guard(&d.key, &target));
        assert!(!r.s.link_guard(&d.key, "another-message"));
        let clean = r.open("p-cafe");
        assert!(r.s.local_caution(&clean.key).is_none());
        let id = clean.messages.last().unwrap().id.clone();
        assert!(!r.s.link_guard(&clean.key, &id));
    }

    #[test]
    fn guard_follows_a_danger_verdict() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-scam");
        let id = d.messages[0].id.clone();
        assert!(!r.s.link_guard(&d.key, &id));
        r.checked(&d, (0.0, 0.91, 0.0));
        assert!(r.s.link_guard(&d.key, &id));
        // A dismissed banner still guards; a warning does not.
        r.s.dismiss_triage();
        assert!(r.s.ai_banner(&d.key).is_none());
        assert!(r.s.link_guard(&d.key, &id));
        let n = r.open("p-news");
        r.checked(&n, (0.62, 0.0, 0.0));
        assert!(!r.s.link_guard(&n.key, &n.messages[0].id));
    }

    #[test]
    fn consent_kept_when_accounts_event_drops_an_account() {
        let mut r = ready(&[PERSONAL, WORK], false);
        let d = r.open("p-ai-phish");
        r.checked(&d, (0.97, 0.0, 0.0));
        r.s.apply(SyncEvent::Accounts(vec![WORK.into()]));
        assert!(r.s.ai_opted_in(PERSONAL), "consent survives");
        assert!(r.s.ai.results.is_empty(), "runtime state goes");
        r.drain();
        r.s.remove_account(WORK);
        assert!(!r.s.ai_opted_in(WORK));
        let (sync, ai) = r.drain();
        assert!(
            sync.iter()
                .any(|c| matches!(c, Command::RemoveAccount(a) if a == WORK))
        );
        assert!(
            ai.iter()
                .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == WORK))
        );
        r.s.clear_client();
        assert!(r.s.prefs.ai.accounts.is_empty());
    }

    #[test]
    fn first_reload_after_auth_leaves_starting() {
        let r = rig(Mode::Google, prefs(&[], false));
        let mut s = r.s;
        s.apply(SyncEvent::Auth(AuthState::Starting));
        assert!(r.ai.try_iter().next().is_none());
        s.apply(SyncEvent::Auth(AuthState::Locked));
        s.apply(SyncEvent::Auth(AuthState::Ready));
        let sent: Vec<AiCommand> = r.ai.try_iter().collect();
        assert_eq!(sent.len(), 1);
        assert!(matches!(sent[0], AiCommand::Reload));
        assert_eq!(s.ai.key, KeyState::Unknown);
    }

    #[test]
    fn refresh_also_reloads_the_key() {
        let r = rig(Mode::Google, prefs(&[], false));
        r.s.refresh();
        let (sync, ai) = r.drain();
        assert!(matches!(&sync[..], [Command::Refresh]));
        assert!(matches!(&ai[..], [AiCommand::Reload]));
    }

    #[test]
    fn spend_limit_change_reaches_the_worker() {
        let mut r = ready(&[], false);
        r.s.change_prefs(|p| p.ai.spend_limit_cents = 250);
        let (_, ai) = r.drain();
        assert!(matches!(&ai[..], [AiCommand::SpendLimit(250)]));
        r.s.change_prefs(|p| p.ai.spend_limit_cents = 1);
        let (_, ai) = r.drain();
        assert!(matches!(&ai[..], [AiCommand::SpendLimit(10)]), "clamped");
    }

    #[test]
    fn dropped_clears_checking_and_reopen_rechecks() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        assert_eq!(r.checks().len(), 1);
        let id = d.messages[0].id.clone();
        r.s.apply_ai(AiEvent::Checking {
            thread: d.key.clone(),
            message_id: id.clone(),
        });
        assert!(r.s.ai_checking(&d.key));
        r.s.apply_ai(AiEvent::Dropped {
            thread: d.key.clone(),
            message_id: id,
        });
        assert!(!r.s.ai_checking(&d.key));
        r.s.open_thread(d.key.clone());
        assert_eq!(r.checks().len(), 1);
    }

    #[test]
    fn failures_retry_only_when_retryable() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-fail");
        r.checks();
        let id = d.messages[0].id.clone();
        r.s.apply_ai(AiEvent::Failed {
            thread: d.key.clone(),
            message_id: id.clone(),
            error: "Couldn't check this message".into(),
            retryable: false,
        });
        r.s.open_thread(d.key.clone());
        assert!(r.checks().is_empty());
        r.s.apply_ai(AiEvent::Failed {
            thread: d.key.clone(),
            message_id: id,
            error: "Couldn't check this message".into(),
            retryable: true,
        });
        assert_eq!(r.s.ai_failure(&d.key).map(|f| f.1), Some(true));
        r.s.retry_ai();
        assert_eq!(r.checks().len(), 1);
        assert!(r.s.ai_failure(&d.key).is_none());
    }

    #[test]
    fn paused_clears_checking() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        r.s.apply_ai(AiEvent::Checking {
            thread: d.key.clone(),
            message_id: d.messages[0].id.clone(),
        });
        r.s.apply_ai(AiEvent::Paused(Some(
            "Your OpenRouter credit has run out".into(),
        )));
        assert!(r.s.ai.checking.is_empty());
        r.checks();
        // While paused, opening sends nothing.
        r.open("p-ai-scam");
        assert!(r.checks().is_empty());
        r.s.apply_ai(AiEvent::Paused(None));
        assert_eq!(r.checks().len(), 1);
    }

    #[test]
    fn ai_rev_bumps_on_prefs_key_and_pause() {
        let mut r = ready(&[], false);
        let mut last = r.s.ai.ai_rev;
        let mut bumped = |s: &Store| {
            let b = s.ai.ai_rev > last;
            last = s.ai.ai_rev;
            b
        };
        r.s.change_prefs(|p| p.ai.background = true);
        assert!(bumped(&r.s));
        r.s.change_prefs(|p| p.clock_24h = !p.clock_24h);
        assert!(!bumped(&r.s));
        r.s.apply_ai(AiEvent::Key(KeyState::Locked));
        assert!(bumped(&r.s));
        r.s.apply_ai(AiEvent::Paused(Some("x".into())));
        assert!(bumped(&r.s));
    }

    #[test]
    fn background_prefetch_respects_hourly_cap() {
        let mut r = ready(&[PERSONAL], true);
        let now = Instant::now();
        r.s.ai.hourly.insert(
            PERSONAL.into(),
            std::iter::repeat_n(now, BACKGROUND_PER_HOUR - 1).collect(),
        );
        new_mail(&mut r, &["p-ai-phish", "p-ai-scam"]);
        assert_eq!(r.prefetches().len(), 1);
        new_mail(&mut r, &["p-ai-spam"]);
        assert!(r.prefetches().is_empty());
        // An hour later there is room again.
        r.s.ai.hourly.insert(
            PERSONAL.into(),
            std::iter::repeat_n(now - HOUR, BACKGROUND_PER_HOUR).collect(),
        );
        new_mail(&mut r, &["p-ai-spam"]);
        assert_eq!(r.prefetches().len(), 1);
    }

    #[test]
    fn prefetch_detail_checked_but_not_cached() {
        let mut r = ready(&[PERSONAL], true);
        new_mail(&mut r, &["p-ai-phish"]);
        let key = ThreadKey::new(PERSONAL, "p-ai-phish");
        assert_eq!(r.prefetches(), std::slice::from_ref(&key));
        r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
        assert!(!r.s.details.contains_key(&key));
        let checks = r.checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].priority, Priority::Background);
        assert!(checks[0].labels.is_none());
        assert!(r.s.ai.prefetching.is_empty());
    }

    #[test]
    fn prefetch_survives_the_synced_after_new_mail() {
        let mut r = ready(&[PERSONAL], true);
        new_mail(&mut r, &["p-ai-phish"]);
        r.s.apply(SyncEvent::Synced(Timestamp::now()));
        assert_eq!(r.s.ai.prefetching.len(), 1);
        r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
        assert_eq!(r.checks().len(), 1);
        // One older than five minutes goes on the next Synced.
        r.s.ai.prefetching.insert(
            ThreadKey::new(PERSONAL, "old"),
            Instant::now() - PREFETCH_TTL,
        );
        r.s.apply(SyncEvent::Synced(Timestamp::now()));
        assert!(r.s.ai.prefetching.is_empty());
    }

    #[test]
    fn background_off_drops_pending_prefetch() {
        let mut r = ready(&[PERSONAL], true);
        new_mail(&mut r, &["p-ai-phish"]);
        r.drain();
        r.s.change_prefs(|p| p.ai.background = false);
        assert!(r.s.ai.prefetching.is_empty());
        r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
        assert!(r.checks().is_empty());
    }

    #[test]
    fn prefetch_rechecks_consent_on_arrival() {
        for change in 0..3 {
            let mut r = ready(&[PERSONAL], true);
            new_mail(&mut r, &["p-ai-phish"]);
            match change {
                0 => r.s.change_prefs(|p| {
                    p.ai.accounts.clear();
                }),
                1 => r.s.apply_ai(AiEvent::Key(KeyState::Missing)),
                _ => r.s.apply_ai(AiEvent::Paused(Some("x".into()))),
            }
            r.drain();
            r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
            assert!(r.checks().is_empty(), "change {change}");
        }
    }

    #[test]
    fn for_tests_sends_no_prefetch() {
        let (mut s, rx) = super::store();
        s.apply(SyncEvent::NewMail {
            account: ME.into(),
            threads: vec![summary(1, &["INBOX", "UNREAD"])],
        });
        assert!(!rx.try_iter().any(|c| matches!(c, Command::Prefetch(_))));
    }

    #[test]
    fn late_open_detail_not_checked_in_background() {
        let mut r = ready(&[PERSONAL], true);
        let a = demo("p-ai-phish");
        r.s.open_thread(a.key.clone());
        r.open("p-ai-scam");
        r.checks();
        // A's detail lands after the user moved on: cached, not checked.
        r.s.apply(SyncEvent::ThreadDetail(a.clone()));
        assert!(r.s.details.contains_key(&a.key));
        assert!(r.checks().is_empty());
    }

    #[test]
    fn no_prefetch_without_key_or_while_paused() {
        let mut r = rig(Mode::Google, prefs(&[PERSONAL], true));
        new_mail(&mut r, &["p-ai-phish"]);
        assert!(r.prefetches().is_empty());
        r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
        r.s.apply_ai(AiEvent::Paused(Some("x".into())));
        new_mail(&mut r, &["p-ai-phish"]);
        assert!(r.prefetches().is_empty());
        r.s.apply_ai(AiEvent::Paused(None));
        new_mail(&mut r, &["p-ai-phish"]);
        assert_eq!(r.prefetches().len(), 1);
    }

    #[test]
    fn key_removed_clears_results() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        r.checked(&d, (0.97, 0.0, 0.0));
        assert!(r.s.ai_badge(&d.key).is_some());
        r.s.apply_ai(AiEvent::Key(KeyState::Missing));
        assert!(r.s.ai.results.is_empty() && r.s.ai.by_thread.is_empty());
        assert!(r.s.ai_badge(&d.key).is_none());
    }

    #[test]
    fn account_reset_forgets_the_account() {
        let mut r = ready(&[PERSONAL], false);
        let d = r.open("p-ai-phish");
        r.checked(&d, (0.97, 0.0, 0.0));
        r.drain();
        r.s.apply(SyncEvent::AccountReset(PERSONAL.into()));
        let (_, ai) = r.drain();
        assert!(
            ai.iter()
                .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == PERSONAL))
        );
        assert!(r.s.ai.results.is_empty());
        assert!(r.s.ai_opted_in(PERSONAL));
    }

    #[test]
    fn results_are_bounded() {
        let mut r = ready(&[PERSONAL], false);
        for i in 0..AI_RESULTS + 5 {
            r.s.apply_ai(AiEvent::Checked {
                thread: ThreadKey::new(PERSONAL, format!("t{i}")),
                message_id: format!("m{i}"),
                outcome: Outcome {
                    verdict: None,
                    labels: None,
                },
            });
        }
        assert_eq!(r.s.ai.results.len(), AI_RESULTS);
        assert!(
            !r.s.ai
                .results
                .contains_key(&(PERSONAL.to_string(), "m0".to_string()))
        );
    }

    #[test]
    fn demo_store_opts_in_both_accounts() {
        let (tx, _rx) = mpsc::channel();
        let s = Store::new(
            SyncHandle::from_sender(tx),
            AiHandle::detached(),
            Mode::Demo,
            prefs(&[], false),
        );
        assert!(s.ai_opted_in(PERSONAL) && s.ai_opted_in(WORK));
        assert!(s.prefs.ai.background);
    }
}

/// Pumps worker events into the store until `done` or the deadline.
fn pump(
    s: &mut Store,
    rx: &async_channel::Receiver<SyncEvent>,
    timeout: Duration,
    mut done: impl FnMut(&Store, &SyncEvent) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => {
                let ev2 = ev.clone();
                s.apply(ev);
                if done(s, &ev2) {
                    return true;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    false
}

fn finished(op: OpId) -> impl FnMut(&Store, &SyncEvent) -> bool {
    move |_, ev| matches!(ev, SyncEvent::MutationFinished { op: o, .. } if *o == op)
}

#[test]
fn demo_end_to_end() {
    let prefs = Prefs {
        ephemeral: true,
        ..Prefs::default()
    };
    let (sync, events) = crate::demo::start(&prefs).unwrap();
    let (ai, ai_events) = crate::ai::start(Mode::Demo, None, &prefs).unwrap();
    let mut ai_prefs = prefs.clone();
    ai_prefs
        .ai
        .accounts
        .insert(ME.into(), crate::ai::DISCLOSURE_VERSION);
    let mut s = Store::for_tests_with_ai(sync.clone(), ai.clone(), Mode::Demo, ai_prefs);
    let page_size = s.prefs.page_size as usize;
    let inbox = view("INBOX");
    pump(&mut s, &events, Duration::from_secs(2), |s, _| {
        s.lists.get(&inbox).is_some_and(|l| !l.ids.is_empty())
    });
    assert_eq!(s.auth, AuthState::Demo);
    assert_eq!(s.accounts.len(), 2);
    assert!(s.unread_badge(ME, "INBOX").is_some_and(|n| n > 0));
    assert_eq!(s.view, Some(inbox.clone()));
    // The inbox leaves nudges (old unanswered mail) to their own list.
    assert_eq!(s.lists[&inbox].ids.len(), page_size);
    let nudges = s.lists[&inbox]
        .ids
        .iter()
        .filter(|id| s.nudge_of(&s.threads[&key(id)]).is_some())
        .count();
    assert_eq!(s.visible_rows().len(), page_size - nudges);
    assert!(s.lists[&inbox].has_more);

    // Open the cursor row: a detail arrives and the thread is marked read.
    let first = s.cursor.clone().unwrap();
    assert!(s.threads[&first].unread());
    s.open_cursor();
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
        matches!(s.details.get(&first), Some(DetailState::Loaded(_)))
    }));
    assert!(!s.threads[&first].unread());
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| s
        .pending
        .is_empty()));
    assert!(!s.threads[&first].unread(), "the worker agrees");

    // Archive removes it from INBOX and the worker confirms.
    let index = s.cursor_index().unwrap();
    let op = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    assert!(!s.lists[&inbox].ids.contains(&first.id));
    assert_ne!(s.open, Some(first.clone()));
    assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
    assert!(!s.threads[&first].in_label("INBOX"));

    // Undo puts it back where it was.
    assert!(s.undo());
    assert_eq!(
        s.lists[&inbox].ids.iter().position(|id| *id == first.id),
        Some(index)
    );
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| s
        .pending
        .is_empty()));
    assert!(s.threads[&first].in_label("INBOX"));
    assert!(s.lists[&inbox].ids.contains(&first.id));

    // A "[fail]" thread rolls back.
    let fail = s.lists[&inbox]
        .ids
        .iter()
        .map(|id| key(id))
        .find(|k| s.threads[k].subject.contains("[fail]"))
        .expect("a [fail] thread on the first page");
    let fail_index = s.lists[&inbox].ids.iter().position(|id| *id == fail.id);
    s.close_thread();
    s.toggle_select(fail.clone());
    let op = s
        .mutate(MutationKind::Labels(LabelDelta::archive()))
        .unwrap();
    assert!(!s.lists[&inbox].ids.contains(&fail.id));
    assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
    assert_eq!(
        s.lists[&inbox].ids.iter().position(|id| *id == fail.id),
        fail_index
    );
    assert!(s.threads[&fail].in_label("INBOX"));
    assert!(
        s.notice
            .as_ref()
            .unwrap()
            .text
            .starts_with("Couldn't archive")
    );

    // A search returns only matching rows.
    let search = ViewKey {
        account: ME.into(),
        kind: ViewKind::Search("from:zoë".into()),
    };
    s.set_view(search.clone());
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
        s.lists.get(&search).is_some_and(|l| !l.loading)
    }));
    let rows = s.visible_rows();
    assert!(!rows.is_empty());
    for row in rows {
        let Row::Thread(t) = row else {
            panic!("skeleton in a demo search");
        };
        assert!(
            t.participants.iter().any(|(a, _)| a
                .name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("zoë")),
            "{} does not match",
            t.key
        );
    }

    // Sending a reply appends to the thread, and the open detail is fetched again.
    let detail = match s.details.get(&first) {
        Some(DetailState::Loaded(d)) => d.clone(),
        other => panic!("no detail for the first thread: {other:?}"),
    };
    let before = detail.messages.len();
    let replied_to = crate::mail::reply::target(&detail).and_then(|m| m.message_id.clone());
    let out = crate::mail::reply::prefill(
        &detail,
        crate::model::ComposeKind::Reply,
        &Address::new(None, ME),
        &|_| "today".into(),
    )
    .unwrap();
    let op = s.send(out);
    assert_eq!(s.send_state(op), Some(&SendState::Sending));
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
        s.send_state(op) == Some(&SendState::Sent)
    }));
    assert_eq!(s.notice.as_ref().unwrap().text, "Message sent");
    assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
        matches!(s.details.get(&first), Some(DetailState::Loaded(d)) if d.messages.len() == before + 1)
    }));
    assert_eq!(s.threads[&first].message_count as usize, before + 1);
    let Some(DetailState::Loaded(d)) = s.details.get(&first) else {
        unreachable!()
    };
    let sent = d.messages.last().unwrap();
    assert!(sent.label_ids.contains(labels::SENT));
    assert_eq!(sent.in_reply_to, replied_to);
    assert!(sent.body.contains("wrote:"));
    assert!(s.threads[&first].in_label(labels::SENT));

    // AI triage with the offline checker (SPEC 16.15): the phishing demo is Danger and
    // arms the link guard.
    let open_and_check = |s: &mut Store, id: &str| -> (ThreadKey, String) {
        let key = key(id);
        s.open_thread(key.clone());
        assert!(pump(s, &events, Duration::from_secs(2), |s, _| {
            matches!(s.details.get(&key), Some(DetailState::Loaded(_)))
        }));
        let target = match s.details.get(&key) {
            Some(DetailState::Loaded(d)) => s.ai_target(d).unwrap().id.clone(),
            _ => unreachable!(),
        };
        let mk = (ME.to_string(), target.clone());
        assert!(
            pump_ai(s, &ai_events, Duration::from_secs(3), |s| {
                s.ai.results.contains_key(&mk)
            }),
            "no verdict for {id}"
        );
        (key, target)
    };
    let (phish, target) = open_and_check(&mut s, "p-ai-phish");
    let v = s.ai_banner(&phish).copied().unwrap();
    assert_eq!(
        (v.level, v.kind),
        (Level::Danger, crate::ai::jev::Kind::Phishing)
    );
    assert!(s.link_guard(&phish, &target));
    assert_eq!(s.ai_badge(&phish), Some(Level::Danger));
    // The crafted notice talks the checker down, but the local caution still guards.
    let (notice, target) = open_and_check(&mut s, "p-ai-notice");
    let result = &s.ai.results[&(ME.to_string(), target.clone())];
    assert_eq!(result.verdict.map(|v| v.level), Some(Level::None));
    assert!(s.ai_banner(&notice).is_none());
    assert!(s.local_caution(&notice).is_some());
    assert!(s.link_guard(&notice, &target));
    assert!(s.suggestions(&notice).is_empty(), "risky mail gets none");
    // (M17) The order receipt suggests Receipts; applying it files the thread and the
    // chip goes, and undo brings both back.
    let (order, _) = open_and_check(&mut s, "p-ai-order");
    let names =
        |s: &Store| -> Vec<String> { s.suggestions(&order).into_iter().map(|x| x.name).collect() };
    assert_eq!(names(&s), ["Receipts"]);
    let op = s.apply_suggestion(0).expect("a suggestion to apply");
    assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
    assert!(
        s.threads[&order]
            .label_ids
            .contains(crate::demo_data::RECEIPTS)
    );
    assert!(names(&s).is_empty());
    assert!(s.undo());
    assert!(
        !s.threads[&order]
            .label_ids
            .contains(crate::demo_data::RECEIPTS)
    );
    assert_eq!(names(&s), ["Receipts"]);
    sync.send(Command::Shutdown);
    ai.send(AiCommand::Shutdown);
}

/// Pumps AI worker events into the store until `done` or the deadline.
fn pump_ai(
    s: &mut Store,
    rx: &async_channel::Receiver<AiEvent>,
    timeout: Duration,
    done: impl Fn(&Store) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => {
                s.apply_ai(ev);
                if done(s) {
                    return true;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    false
}

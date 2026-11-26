use std::sync::mpsc;

use jiff::SignedDuration;

use super::*;
use crate::model::{Address, LastMessage};

const ME: &str = "you@example.com";

fn store() -> (Store, mpsc::Receiver<Command>) {
    let (tx, rx) = mpsc::channel();
    let mut s = Store::for_tests(SyncHandle::from_sender(tx));
    s.apply(SyncEvent::Accounts(vec![ME.into()]));
    (s, rx)
}

fn view(kind: ViewKind) -> ViewKey {
    ViewKey {
        account: ME.into(),
        kind,
    }
}

/// A thread whose newest message is `days` old.
fn thread(id: &str, days: i64, labels: &[&str], from_me: bool) -> ThreadSummary {
    ThreadSummary {
        key: ThreadKey::new(ME, id),
        history_id: 1,
        subject: id.into(),
        snippet: String::new(),
        participants: vec![
            (Address::new(None, "ann@example.org"), false),
            (Address::new(None, ME), false),
        ],
        last_date: Timestamp::now() - SignedDuration::from_hours(days * 24),
        message_count: 1,
        label_ids: labels.iter().map(|s| s.to_string()).collect(),
        has_attachments: false,
        last: Some(LastMessage {
            sender: Address::new(None, if from_me { ME } else { "ann@example.org" }),
            from_me,
        }),
    }
}

fn load(s: &mut Store, kind: ViewKind, threads: Vec<ThreadSummary>) {
    s.apply(SyncEvent::ThreadPage {
        view: view(kind),
        ids: threads.iter().map(|t| t.key.id.clone()).collect(),
        threads,
        mode: PageMode::Replace,
        has_more: false,
    });
}

fn shown(s: &Store, kind: ViewKind) -> Vec<String> {
    s.shown_ids(&view(kind))
}

/// Inbox: fresh mail, an old unanswered message and an old promotion. Nudges: the same plus
/// a sent message nobody answered.
fn loaded() -> (Store, mpsc::Receiver<Command>) {
    let (mut s, rx) = store();
    let inbox = vec![
        thread("fresh", 1, &["INBOX"], false),
        thread("old", 5, &["INBOX"], false),
        thread("promo", 5, &["INBOX", "CATEGORY_PROMOTIONS"], false),
    ];
    load(&mut s, ViewKind::Label("INBOX".into()), inbox.clone());
    let mut candidates = inbox;
    candidates.push(thread("sent", 6, &["SENT"], true));
    load(&mut s, ViewKind::Nudges, candidates);
    while rx.try_recv().is_ok() {}
    (s, rx)
}

#[test]
fn nudges_list_keeps_only_real_nudges_and_the_inbox_leaves_them_out() {
    let (s, _rx) = loaded();
    assert_eq!(shown(&s, ViewKind::Nudges), ["old", "sent"]);
    assert_eq!(
        shown(&s, ViewKind::Label("INBOX".into())),
        ["fresh", "promo"]
    );
    assert_eq!(s.nudge_count(ME), 2);
}

#[test]
fn the_sidebar_lists_nudges_after_the_inbox_with_a_count() {
    let (mut s, _rx) = loaded();
    s.apply(SyncEvent::Labels {
        account: ME.into(),
        labels: vec![],
    });
    let entries = s.labels_for(ME);
    let at = entries
        .iter()
        .position(|e| e.view == ViewKind::Nudges)
        .expect("a Nudges entry");
    assert_eq!(entries[at - 1].view, ViewKind::Label("INBOX".into()));
    assert_eq!(entries[at].count, Some(2));
}

#[test]
fn dismissing_removes_the_nudge_tells_the_worker_and_returns_it_to_the_inbox() {
    let (mut s, rx) = loaded();
    s.set_view(view(ViewKind::Nudges));
    let _ = rx.try_iter().count();
    s.cursor = Some(ThreadKey::new(ME, "old"));
    assert_eq!(s.dismiss_nudges(), 1);
    assert_eq!(shown(&s, ViewKind::Nudges), ["sent"]);
    assert_eq!(s.cursor, Some(ThreadKey::new(ME, "sent")));
    assert!(shown(&s, ViewKind::Label("INBOX".into())).contains(&"old".to_string()));
    let date = s.threads[&ThreadKey::new(ME, "old")]
        .last_date
        .as_millisecond();
    let Some(Command::DismissNudges { account, entries }) = rx.try_iter().next() else {
        panic!("expected DismissNudges");
    };
    assert_eq!(account, ME);
    assert_eq!(entries, [("old".to_string(), date)]);
}

#[test]
fn dismissing_a_non_nudge_does_nothing() {
    let (mut s, rx) = loaded();
    s.cursor = Some(ThreadKey::new(ME, "fresh"));
    assert_eq!(s.dismiss_nudges(), 0);
    assert!(rx.try_iter().next().is_none());
}

#[test]
fn a_new_message_makes_a_dismissed_thread_a_nudge_again() {
    let (mut s, _rx) = loaded();
    s.cursor = Some(ThreadKey::new(ME, "old"));
    s.dismiss_nudges();
    // The same thread with a newer (still old enough) message.
    let mut again = thread("old", 4, &["INBOX"], false);
    again.history_id = 2;
    s.apply(SyncEvent::ThreadsUpdated(vec![again]));
    assert_eq!(shown(&s, ViewKind::Nudges), ["old", "sent"]);
}

#[test]
fn the_workers_dismissals_replace_the_stores() {
    let (mut s, _rx) = loaded();
    let date = s.threads[&ThreadKey::new(ME, "sent")]
        .last_date
        .as_millisecond();
    s.apply(SyncEvent::NudgeDismissals {
        account: ME.into(),
        dismissed: [("sent".to_string(), date)].into(),
    });
    assert_eq!(shown(&s, ViewKind::Nudges), ["old"]);
}

#[test]
fn archiving_a_nudge_takes_it_out() {
    let (mut s, _rx) = loaded();
    s.set_view(view(ViewKind::Nudges));
    s.cursor = Some(ThreadKey::new(ME, "old"));
    s.mutate(MutationKind::Labels(LabelDelta::archive()));
    assert_eq!(shown(&s, ViewKind::Nudges), ["sent"]);
}

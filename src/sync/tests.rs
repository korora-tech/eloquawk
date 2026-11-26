use super::*;
use crate::google::fake::{FakeServer, Reply};
use crate::google::gmail::fixtures;
use crate::secrets::SecretStore;

const PROFILE: &str = r#"{"emailAddress":"me@example.com","historyId":"42"}"#;
const TOKEN: &str = r#"{"access_token":"at2","expires_in":3599,"token_type":"Bearer"}"#;

/// Short timings so the poll and the backoff happen within a test.
fn fast() -> Tuning {
    Tuning {
        refresh: Duration::from_millis(300),
        page_size: 2,
        backoff_base: Duration::from_millis(30),
        backoff_cap: Duration::from_millis(200),
        jitter_max: Duration::from_millis(5),
    }
}

fn client() -> ClientCredentials {
    ClientCredentials {
        client_id: "c.apps.googleusercontent.com".into(),
        client_secret: "cs".into(),
    }
}

/// Endpoints on a port nobody listens on, for tests that must not make a request.
fn nowhere() -> Endpoints {
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    Endpoints {
        base: base.clone(),
        upload: base.clone(),
        token: format!("{base}/token"),
        revoke: format!("{base}/revoke"),
    }
}

fn fake_endpoints(server: &FakeServer) -> Endpoints {
    Endpoints {
        base: server.url("/gmail"),
        upload: server.url("/upload"),
        token: server.url("/token"),
        revoke: server.url("/revoke"),
    }
}

/// A worker that is not running on a thread, and its event receiver.
fn worker(secrets: Secrets, endpoints: Endpoints) -> (Worker, async_channel::Receiver<SyncEvent>) {
    let (etx, erx) = async_channel::unbounded();
    let (tx, _rx) = mpsc::channel();
    let mut w = Worker::new(secrets, etx, tx, endpoints, Some(client()), fast());
    w.client = Some(client());
    w.auth = AuthState::Ready;
    (w, erx)
}

fn drain(rx: &async_channel::Receiver<SyncEvent>) -> Vec<SyncEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// Receives events from a running worker until `done` matches one, or 5 s pass.
fn until(
    rx: &async_channel::Receiver<SyncEvent>,
    done: impl Fn(&SyncEvent) -> bool,
) -> Vec<SyncEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => {
                let stop = done(&ev);
                out.push(ev);
                if stop {
                    return out;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    panic!("timed out; got {out:?}");
}

fn problem(events: &[SyncEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        SyncEvent::Problem(p) => Some(p.clone().unwrap_or_default()),
        _ => None,
    })
}

fn signing_in(w: &mut Worker) -> u64 {
    w.sign_in_seq += 1;
    w.sign_in = Some(SignIn {
        seq: w.sign_in_seq,
        cancel: Arc::new(AtomicBool::new(false)),
    });
    w.sign_in_seq
}

fn tokens(scope: Option<&str>) -> TokenResponse {
    TokenResponse {
        access_token: "at".into(),
        expires_in: 3599,
        refresh_token: Some("rt".into()),
        scope: scope.map(str::to_string),
    }
}

#[test]
fn no_client_at_all_needs_one_and_pasting_saves_it() {
    let secrets = Secrets::memory();
    let (handle, rx) = start_with(secrets.clone(), nowhere(), None, fast(), None, None).unwrap();
    let events = until(&rx, |e| {
        matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
    });
    assert!(matches!(events[0], SyncEvent::Client(None)));

    // A secret is required for a client id that is not in use.
    handle.send(Command::SaveClient {
        client_id: "pasted.apps.googleusercontent.com".into(),
        client_secret: None,
    });
    let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
    assert!(problem(&events).unwrap().contains("client secret"));
    assert_eq!(secrets.client().unwrap(), None);

    handle.send(Command::SaveClient {
        client_id: " pasted.apps.googleusercontent.com ".into(),
        client_secret: Some(" GOCSPX-pasted ".into()),
    });
    let events = until(&rx, |e| matches!(e, SyncEvent::Auth(AuthState::Ready)));
    assert!(events.iter().any(|e| matches!(
        e,
        SyncEvent::Client(Some(id)) if id == "pasted.apps.googleusercontent.com"
    )));
    assert_eq!(
        secrets.client().unwrap(),
        Some(ClientCredentials {
            client_id: "pasted.apps.googleusercontent.com".into(),
            client_secret: "GOCSPX-pasted".into(),
        })
    );

    // "Use built-in" without a built-in client goes back to asking for one.
    handle.send(Command::ClearClient);
    until(&rx, |e| {
        matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
    });
    assert_eq!(secrets.client().unwrap(), None);
    handle.send(Command::Shutdown);
}

#[test]
fn builtin_client_starts_ready_without_accounts() {
    let (handle, rx) = start_with(
        Secrets::memory(),
        nowhere(),
        Some(client()),
        fast(),
        None,
        None,
    )
    .unwrap();
    let events = until(&rx, |e| matches!(e, SyncEvent::Auth(_)));
    assert!(matches!(events[0], SyncEvent::Client(None)));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SyncEvent::Accounts(a) if a.is_empty()))
    );
    assert!(matches!(
        events.last(),
        Some(SyncEvent::Auth(AuthState::Ready))
    ));
    handle.send(Command::Shutdown);
}

#[test]
fn prefetch_queues_a_background_job_for_known_accounts() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    w.queue = Queue::default();
    let key = ThreadKey::new("me@example.com", "t");
    w.handle(Command::Prefetch(key.clone()));
    w.handle(Command::Prefetch(ThreadKey::new("gone@example.com", "t")));
    assert!(w.queue.interactive.is_empty());
    assert_eq!(w.queue.background, [Job::Prefetch(key)]);
}

#[test]
fn reads_and_changes_queue_interactive_jobs() {
    let (mut w, rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let inbox = ViewKey {
        account: "me@example.com".into(),
        kind: ViewKind::Label("INBOX".into()),
    };
    w.handle(Command::Want {
        view: inbox.clone(),
        more: false,
    });
    w.handle(Command::OpenThread(ThreadKey::new("me@example.com", "t")));
    w.handle(Command::Trash {
        op: 3,
        account: "me@example.com".into(),
        thread_ids: vec!["t".into()],
    });
    assert_eq!(w.want.as_ref(), Some(&inbox));
    assert_eq!(
        w.queue.interactive,
        [
            Job::ListView(inbox, false),
            Job::OpenThread(ThreadKey::new("me@example.com", "t")),
            Job::Mutate(Mutation {
                op: 3,
                account: "me@example.com".into(),
                thread_ids: vec!["t".into()],
                change: Change::Trash,
            }),
        ]
    );
    // A view, a change or a send for an account that is not signed in answers at once.
    w.handle(Command::Want {
        view: ViewKey {
            account: "gone@example.com".into(),
            kind: ViewKind::AllMail,
        },
        more: false,
    });
    w.handle(Command::Modify {
        op: 4,
        account: "gone@example.com".into(),
        thread_ids: vec!["t".into()],
        delta: LabelDelta::archive(),
    });
    w.handle(Command::Send {
        op: 5,
        outgoing: outgoing("gone@example.com", None),
    });
    let events = drain(&rx);
    assert!(matches!(
        &events[0],
        SyncEvent::ThreadPage { ids, has_more: false, mode: PageMode::Replace, .. } if ids.is_empty()
    ));
    assert!(matches!(
        &events[1],
        SyncEvent::MutationFinished { op: 4, error: Some(e) } if e == SIGNED_OUT
    ));
    assert!(matches!(
        &events[2],
        SyncEvent::SendFinished {
            op: 5,
            error: Some(SendError {
                maybe_sent: false,
                ..
            })
        }
    ));
    assert_eq!(w.queue.interactive.len(), 3);
}

#[test]
fn backoff_doubles_up_to_the_cap_and_honours_hints() {
    let t = Tuning::new(60, 50);
    let now = Instant::now();
    let mut b = Backoff::default();
    let mut waits = Vec::new();
    // Each failure comes once the previous wait is over.
    let mut at = now;
    for _ in 0..8 {
        b.failed(at, None, Duration::ZERO, &t);
        waits.push(b.retry_at.unwrap() - at);
        at = b.retry_at.unwrap();
    }
    let secs: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
    assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300]);
    assert!(b.waiting(at - Duration::from_secs(1)));
    assert!(!b.waiting(at));

    // Jitter is added on top; a longer hint wins over the schedule.
    b.reset();
    b.failed(now, None, Duration::from_millis(400), &t);
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_millis(5400));
    b.reset();
    b.failed(now, Some(Duration::from_secs(90)), Duration::ZERO, &t);
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(90));

    // An HTTP-date Retry-After, through the api classification.
    let now_ts: Timestamp = "2015-10-21T07:27:00Z".parse().unwrap();
    let e = api::classify(429, Some("Wed, 21 Oct 2015 07:28:00 GMT"), "{}", now_ts);
    let ApiError::RateLimited { retry_after } = e else {
        panic!("{e:?}");
    };
    b.reset();
    b.failed(now, retry_after, Duration::ZERO, &t);
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(60));

    // Quota: at least 15 minutes, longer when Google says so.
    b.reset();
    b.quota(now, None);
    assert_eq!(b.retry_at.unwrap() - now, QUOTA_WAIT);
    b.quota(now, Some(Duration::from_secs(3600)));
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(3600));
    b.reset();
    assert_eq!(b, Backoff::default());
}

#[test]
fn jitter_stays_below_its_bound() {
    let t = fast();
    for _ in 0..50 {
        assert!(t.jitter() < t.jitter_max);
    }
    let none = Tuning {
        jitter_max: Duration::ZERO,
        ..t
    };
    assert_eq!(none.jitter(), Duration::ZERO);
}

#[test]
fn background_jobs_in_backoff_are_parked_then_requeued() {
    let (mut w, rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let now = Instant::now();
    let retry_at = now + Duration::from_secs(20);
    w.accounts
        .get_mut("me@example.com")
        .unwrap()
        .backoff
        .retry_at = Some(retry_at);
    let chunk = Job::FetchMeta("me@example.com".into(), vec!["a".into(), "b".into()]);
    w.queue.push(chunk.clone());
    w.queue
        .push(Job::Labels("me@example.com".into(), vec!["INBOX".into()]));
    w.step(now);
    w.step(now);
    assert!(w.queue.is_empty());
    assert!(drain(&rx).is_empty(), "nothing ran");
    let parked = &w.accounts["me@example.com"].deferred;
    assert_eq!(parked.len(), 2);
    assert_eq!(parked[0], chunk);
    // The loop wakes for the retry, not later (and never sleeps past 30 s).
    w.next_poll = now + Duration::from_secs(3600);
    assert_eq!(w.wait(now), Duration::from_secs(20));

    w.requeue_deferred(now + Duration::from_secs(5));
    assert!(w.queue.is_empty(), "still waiting");
    w.requeue_deferred(retry_at);
    assert_eq!(w.queue.background.front(), Some(&chunk));
    assert_eq!(w.queue.background.len(), 2);
    assert!(w.accounts["me@example.com"].deferred.is_empty());
    assert_eq!(w.wait(now), Duration::ZERO);
}

#[test]
fn queue_coalesces_and_runs_interactive_jobs_first() {
    let a = || "me@example.com".to_string();
    let key = ThreadKey::new("me@example.com", "t");
    let view = ViewKey {
        account: a(),
        kind: ViewKind::AllMail,
    };
    let mut q = Queue::default();
    q.push(Job::InitialSync(a()));
    q.push(Job::Labels(a(), vec!["INBOX".into()]));
    q.push(Job::InitialSync(a()));
    q.push(Job::OpenThread(key.clone()));
    q.push(Job::ListView(view.clone(), false));
    q.push(Job::ListView(view.clone(), false));
    q.push_soon(vec![
        Job::FetchMeta(a(), vec!["1".into()]),
        Job::FetchMeta(a(), vec!["2".into()]),
    ]);
    q.push_soon(vec![Job::FetchMeta(a(), vec!["1".into()])]);
    q.push(Job::History("other@example.com".into()));
    assert_eq!(
        q.queued_meta("me@example.com"),
        HashSet::from(["1".to_string(), "2".to_string()])
    );
    let order: Vec<Job> = std::iter::from_fn(|| q.pop()).collect();
    assert_eq!(
        order,
        [
            Job::OpenThread(key.clone()),
            Job::ListView(view.clone(), false),
            Job::FetchMeta(a(), vec!["1".into()]),
            Job::FetchMeta(a(), vec!["2".into()]),
            Job::InitialSync(a()),
            Job::Labels(a(), vec!["INBOX".into()]),
            Job::History("other@example.com".into()),
        ]
    );

    // A prefetch waits in the background lane, coalesced, behind an open.
    q.push(Job::Prefetch(key.clone()));
    q.push(Job::Prefetch(key.clone()));
    q.push(Job::OpenThread(key.clone()));
    assert_eq!(q.pop(), Some(Job::OpenThread(key.clone())));
    assert_eq!(q.pop(), Some(Job::Prefetch(key.clone())));
    assert!(q.is_empty());

    q.push(Job::OpenThread(key));
    q.push(Job::InitialSync(a()));
    q.push(Job::History("other@example.com".into()));
    q.forget("me@example.com");
    assert_eq!(q.pop(), Some(Job::History("other@example.com".into())));
    assert!(q.is_empty());
}

#[test]
fn long_id_lists_are_chunked_by_ten() {
    let ids: Vec<String> = (0..23).map(|i| i.to_string()).collect();
    let jobs = chunked(&ids, |c| Job::FetchMeta("a".into(), c));
    let sizes: Vec<usize> = jobs
        .iter()
        .map(|j| match j {
            Job::FetchMeta(_, c) => c.len(),
            _ => 0,
        })
        .collect();
    assert_eq!(sizes, [10, 10, 3]);
}

#[test]
fn the_poll_queues_initial_sync_until_one_completes() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let now = Instant::now();
    let poll = |w: &mut Worker| {
        w.queue = Queue::default();
        w.poll(now);
        std::iter::from_fn(|| w.queue.pop()).collect::<Vec<Job>>()
    };
    // Never synced (say `profile` failed): initial sync again.
    assert_eq!(poll(&mut w), [Job::InitialSync("me@example.com".into())]);
    assert_eq!(w.next_poll, now + w.tuning.refresh);
    // A cursor but no completed first sync (listing failed): still initial sync.
    w.accounts.get_mut("me@example.com").unwrap().history_id = Some(7);
    assert_eq!(poll(&mut w), [Job::InitialSync("me@example.com".into())]);
    // After the first sync: a history round, plus the skeleton rows a chunk left behind.
    {
        let account = w.accounts.get_mut("me@example.com").unwrap();
        account.synced_once = true;
        account.missing.insert("t9".into(), 1);
    }
    assert_eq!(
        poll(&mut w),
        [
            Job::History("me@example.com".into()),
            Job::FetchMeta("me@example.com".into(), vec!["t9".into()]),
        ]
    );
    // An account in backoff is left alone.
    w.accounts
        .get_mut("me@example.com")
        .unwrap()
        .backoff
        .retry_at = Some(now + Duration::from_secs(5));
    assert!(poll(&mut w).is_empty());
    // So is everything while the keyring is locked.
    w.accounts
        .get_mut("me@example.com")
        .unwrap()
        .backoff
        .reset();
    w.auth = AuthState::Locked;
    assert!(poll(&mut w).is_empty());
}

#[test]
fn failures_while_waiting_do_not_hurry_the_schedule() {
    let t = Tuning::new(60, 50);
    let now = Instant::now();
    let mut b = Backoff::default();
    b.failed(now, None, Duration::ZERO, &t);
    let first = b.clone();
    // Clicks while offline: the interactive jobs fail again within the wait.
    for i in 1..5 {
        b.failed(now + Duration::from_secs(i), None, Duration::ZERO, &t);
    }
    assert_eq!(b, first, "neither the attempts nor the wait grow");
    // A longer hint still lengthens the wait.
    b.failed(
        now + Duration::from_secs(1),
        Some(Duration::from_secs(30)),
        Duration::ZERO,
        &t,
    );
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(31));
    assert_eq!(b.attempts, 1);
    // Once the wait is over the schedule moves on as before.
    b.failed(now + Duration::from_secs(31), None, Duration::ZERO, &t);
    assert_eq!(b.attempts, 2);
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(41));
    // A quota problem never shortens a longer wait.
    b.quota(now, Some(Duration::from_secs(3600)));
    b.quota(now, None);
    assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(3600));
}

#[test]
fn long_rate_limits_are_quota_problems_that_wait_for_the_hint() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let hour = Duration::from_secs(3600);
    let wait = |w: &Worker| {
        w.accounts["me@example.com"]
            .backoff
            .retry_at
            .unwrap()
            .saturating_duration_since(Instant::now())
    };

    // As `classify` reports it: the Google message and the hint.
    let e = api::classify(
        429,
        Some("3600"),
        r#"{"error":{"message":"slow"}}"#,
        Timestamp::now(),
    );
    assert!(w.failed("me@example.com", &CallError::Api(e)));
    assert!(
        wait(&w) > hour - Duration::from_secs(5),
        "the hint, not 15 minutes"
    );
    assert_eq!(
        w.problems["me@example.com"],
        "me@example.com: Gmail limit reached: slow"
    );

    // A long `RateLimited` from anywhere else gets the same treatment.
    w.accounts
        .get_mut("me@example.com")
        .unwrap()
        .backoff
        .reset();
    w.problems.clear();
    let e = ApiError::RateLimited {
        retry_after: Some(QUOTA_WAIT + Duration::from_secs(60)),
    };
    assert!(w.failed("me@example.com", &CallError::Api(e)));
    assert!(wait(&w) > QUOTA_WAIT);
    assert!(w.problems["me@example.com"].contains("Gmail limit reached"));
    assert_eq!(w.accounts["me@example.com"].backoff.attempts, 1);

    // A short one is a burst.
    w.accounts
        .get_mut("me@example.com")
        .unwrap()
        .backoff
        .reset();
    let e = ApiError::RateLimited {
        retry_after: Some(Duration::from_secs(2)),
    };
    assert!(w.failed("me@example.com", &CallError::Api(e)));
    assert!(wait(&w) < Duration::from_secs(3));
    assert!(w.problems["me@example.com"].contains("slow down"));
}

#[test]
fn a_failed_prefetch_backs_off_without_a_visible_problem() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let e = CallError::Api(ApiError::Http {
        status: 503,
        body: "Backend Error".into(),
    });
    assert!(w.failed_as("me@example.com", &e, false));
    assert_eq!(w.accounts["me@example.com"].backoff.attempts, 1);
    assert!(w.problems.is_empty());
    // The same failure from an open is reported.
    assert!(w.failed("me@example.com", &e));
    assert!(w.problems["me@example.com"].contains("503"));
}

#[test]
fn the_poll_skips_ids_already_queued_or_parked() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let a = || "me@example.com".to_string();
    let now = Instant::now();
    {
        let account = w.accounts.get_mut("me@example.com").unwrap();
        account.synced_once = true;
        account.history_id = Some(1);
        for id in ["t1", "t2", "t3"] {
            account.missing.insert(id.into(), 1);
        }
        account.defer(Job::FetchMeta(a(), vec!["t1".into()]));
    }
    w.queue.push(Job::FetchMeta(a(), vec!["t2".into()]));
    w.poll(now);
    let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
    assert_eq!(
        jobs,
        [
            Job::FetchMeta(a(), vec!["t2".into()]),
            Job::History(a()),
            Job::FetchMeta(a(), vec!["t3".into()]),
        ]
    );

    // A listing takes its ids out of queued chunks and queues them at the front.
    let mut q = Queue::default();
    q.push(Job::FetchMeta(a(), vec!["1".into(), "2".into()]));
    q.push(Job::FetchMeta(a(), vec!["3".into()]));
    q.push(Job::FetchMeta("other".into(), vec!["3".into()]));
    q.strip_meta(&a(), &["2".into(), "3".into()]);
    assert_eq!(
        std::iter::from_fn(|| q.pop()).collect::<Vec<Job>>(),
        [
            Job::FetchMeta(a(), vec!["1".into()]),
            Job::FetchMeta("other".into(), vec!["3".into()]),
        ]
    );
}

#[test]
fn refused_threads_wait_for_a_new_history_id() {
    let listed = |h: u32| format!(r#"{{"threads":[{{"id":"t1","historyId":"{h}"}}]}}"#);
    let server = FakeServer::start(vec![
        Reply::json(200, &listed(5)).for_path("/gmail/threads"),
        Reply::json(403, r#"{"error":{"code":403,"message":"nope"}}"#)
            .for_path("/gmail/threads/t1"),
        Reply::json(200, &listed(5)).for_path("/gmail/threads"),
        Reply::json(200, &listed(6)).for_path("/gmail/threads"),
    ]);
    let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
    let access = Access {
        token: "at".into(),
        expires: Instant::now() + Duration::from_secs(3600),
    };
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), Some(access)),
    );
    let view = ViewKey {
        account: "me@example.com".into(),
        kind: ViewKind::AllMail,
    };
    let meta = |ids: &[&str]| {
        Job::FetchMeta(
            "me@example.com".into(),
            ids.iter().map(|s| s.to_string()).collect(),
        )
    };

    assert!(w.list(&view, false, PageMode::Replace).is_ok());
    assert_eq!(w.queue.pop(), Some(meta(&["t1"])));
    w.fetch_meta("me@example.com", vec!["t1".into()]);
    let account = &w.accounts["me@example.com"];
    assert!(account.missing.is_empty());
    assert_eq!(account.refused.get("t1"), Some(&5));
    assert!(account.deferred.is_empty(), "a refusal is not retried");

    // Neither the poll nor listing the same thread again asks for it.
    w.accounts.get_mut("me@example.com").unwrap().synced_once = true;
    w.accounts.get_mut("me@example.com").unwrap().history_id = Some(1);
    w.poll(Instant::now());
    assert_eq!(w.queue.pop(), Some(Job::History("me@example.com".into())));
    assert!(w.queue.is_empty());
    assert!(w.list(&view, false, PageMode::Refresh).is_ok());
    assert!(w.queue.is_empty());

    // Once it changes it is fetched again.
    assert!(w.list(&view, false, PageMode::Refresh).is_ok());
    assert_eq!(w.queue.pop(), Some(meta(&["t1"])));
    assert!(w.accounts["me@example.com"].refused.is_empty());
    server.finish();
}

#[test]
fn searches_left_behind_drop_their_page_tokens() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    let view = |kind: ViewKind| ViewKey {
        account: "me@example.com".into(),
        kind,
    };
    let search = ViewKind::Search("from:a".into());
    {
        let pages = &mut w.accounts.get_mut("me@example.com").unwrap().pages;
        pages.insert(ViewKind::AllMail, Some("p".into()));
        pages.insert(search.clone(), Some("q".into()));
        pages.insert(ViewKind::Search("old".into()), None);
    }
    w.handle(Command::Want {
        view: view(search.clone()),
        more: false,
    });
    let pages = &w.accounts["me@example.com"].pages;
    assert_eq!(pages.len(), 2);
    assert!(
        pages.contains_key(&search),
        "the shown search keeps its token"
    );
    // Its next page keeps it too; moving away drops it.
    w.handle(Command::Want {
        view: view(search.clone()),
        more: true,
    });
    assert!(w.accounts["me@example.com"].pages.contains_key(&search));
    w.handle(Command::Want {
        view: view(ViewKind::AllMail),
        more: false,
    });
    let pages = &w.accounts["me@example.com"].pages;
    assert_eq!(pages.keys().collect::<Vec<_>>(), [&ViewKind::AllMail]);
}

#[test]
fn refresh_resyncs_and_keeps_following_the_history() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    signed_in(&mut w, "new@example.com", "rt");
    {
        let a = w.accounts.get_mut("me@example.com").unwrap();
        a.synced_once = true;
        a.history_id = Some(7);
        a.backoff.retry_at = Some(Instant::now() + Duration::from_secs(60));
    }
    w.handle(Command::Refresh);
    let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
    assert_eq!(
        jobs,
        [
            Job::InitialSync("me@example.com".into()),
            Job::History("me@example.com".into()),
            Job::InitialSync("new@example.com".into()),
        ]
    );
    assert_eq!(w.accounts["me@example.com"].backoff, Backoff::default());
}

#[test]
fn tune_moves_the_next_poll() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    let last = w.last_poll;
    w.handle(Command::Tune {
        refresh_secs: 900,
        page_size: 25,
    });
    assert_eq!(w.tuning.page_size, 25);
    assert_eq!(w.next_poll, last + Duration::from_secs(900));
}

#[test]
fn signing_out_drops_the_accounts_jobs() {
    let (mut w, _rx) = worker(Secrets::memory(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    w.queue.push(Job::InitialSync("me@example.com".into()));
    w.queue
        .push(Job::OpenThread(ThreadKey::new("me@example.com", "t")));
    assert!(w.remove_account("me@example.com", Revoke::LocalOnly));
    assert!(w.queue.is_empty());
}

const A: &str = "18c0a1b2c3d4e5f6";
const B: &str = "18c0a1b2c3d4e5f7";

/// The metadata fixture, renamed to thread `id`.
fn meta(id: &str) -> String {
    fixtures::THREAD_META.replace(A, id)
}

fn query(r: &crate::google::fake::Recorded) -> Vec<(String, String)> {
    let q = r.target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    url::form_urlencoded::parse(q.as_bytes())
        .into_owned()
        .collect()
}

fn has(q: &[(String, String)], k: &str, v: &str) -> bool {
    q.iter().any(|(a, b)| a == k && b == v)
}

/// SPEC 12.2 `fake_gmail_read_path`: the real worker thread against a scripted Gmail.
#[test]
fn fake_gmail_read_path() {
    let labels: gmail::LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
    let mut script = vec![
        Reply::json(200, TOKEN).for_path("/token"),
        // The first profile fails; the poll queues the initial sync again.
        Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
            .for_path("/gmail/profile"),
        Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
        // Later polls find no history since the cursor.
        Reply::json(200, fixtures::HISTORY_EMPTY)
            .for_path("/gmail/history")
            .sticky(),
        Reply::json(200, fixtures::LABELS).for_path("/gmail/labels"),
        Reply::json(200, fixtures::SEND_AS).for_path("/gmail/settings/sendAs"),
        Reply::json(200, fixtures::THREADS).for_path("/gmail/threads"),
        Reply::json(200, r#"{"threads":[{"id":"t3","historyId":"9"}]}"#).for_path("/gmail/threads"),
        Reply::json(200, &meta(A)).for_path(&format!("/gmail/threads/{A}")),
        Reply::json(
            429,
            r#"{"error":{"code":429,"message":"Too many concurrent requests for user"}}"#,
        )
        .with_header("Retry-After", "0")
        .for_path(&format!("/gmail/threads/{B}")),
        Reply::json(200, &meta(B)).for_path(&format!("/gmail/threads/{B}")),
        Reply::json(200, &meta("t3")).for_path("/gmail/threads/t3"),
        Reply::json(200, fixtures::THREAD_FULL).for_path(&format!("/gmail/threads/{A}")),
    ];
    for l in &labels.labels {
        let body = if l.id == "INBOX" {
            fixtures::LABEL_INBOX.to_string()
        } else {
            format!(
                r#"{{"id":"{}","name":"{}","threadsUnread":1,"threadsTotal":2,"messagesTotal":3}}"#,
                l.id, l.name
            )
        };
        script.push(
            Reply::json(200, &body)
                .for_path(&format!("/gmail/labels/{}", l.id))
                .sticky(),
        );
    }
    let server = FakeServer::start(script);
    let secrets = Secrets::memory();
    secrets.set_token("me@example.com", "rt").unwrap();
    let (handle, rx) = start_with(
        secrets,
        fake_endpoints(&server),
        Some(client()),
        fast(),
        None,
        None,
    )
    .unwrap();

    // Initial sync, after one failed attempt: labels, page 1 with skeletons, metadata
    // (one chunk deferred by a 429, not lost), then the label counts.
    let mut events = until(&rx, |e| matches!(e, SyncEvent::LabelCounts { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SyncEvent::Problem(Some(p)) if p.contains("HTTP 500"))),
        "the failed profile is reported"
    );
    assert_eq!(
        problem(&events).as_deref(),
        Some(""),
        "and cleared by the sync"
    );
    let labels_ev = events
        .iter()
        .find_map(|e| match e {
            SyncEvent::Labels { labels, .. } => Some(labels.clone()),
            _ => None,
        })
        .unwrap();
    assert!(labels_ev.iter().any(|l| l.id == "Label_1"));
    let aliases = events
        .iter()
        .find_map(|e| match e {
            SyncEvent::SendAs { aliases, .. } => Some(aliases.clone()),
            _ => None,
        })
        .expect("the send-as list is fetched");
    // The default first, the unverified alias left out, signatures as plain text.
    let got: Vec<(&str, &str)> = aliases
        .iter()
        .map(|a| (a.email.as_str(), a.signature.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("me@example.com", "Me"),
            ("alias@example.com", "Studio\nTeam")
        ]
    );
    let page = events
        .iter()
        .find_map(|e| match e {
            SyncEvent::ThreadPage {
                ids,
                threads,
                mode,
                has_more,
                ..
            } => Some((ids.clone(), threads.len(), *mode, *has_more)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        page,
        (
            vec![A.to_string(), B.to_string()],
            0,
            PageMode::Replace,
            true
        )
    );
    let updated: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::ThreadsUpdated(t) => Some(t.iter().map(|s| s.key.id.clone()).collect()),
            _ => None,
        })
        .collect();
    assert_eq!(updated, [vec![A.to_string()], vec![B.to_string()]]);
    let SyncEvent::LabelCounts { labels: counts, .. } = events.last().unwrap() else {
        unreachable!()
    };
    let inbox = counts.iter().find(|l| l.id == "INBOX").unwrap();
    assert_eq!((inbox.threads_unread, inbox.threads_total), (3, 42));
    assert!(events.iter().any(|e| matches!(e, SyncEvent::Busy(true))));
    assert!(events.iter().any(|e| matches!(e, SyncEvent::Synced(_))));

    // The next page, with the token Gmail gave.
    let inbox_view = ViewKey {
        account: "me@example.com".into(),
        kind: ViewKind::Label("INBOX".into()),
    };
    handle.send(Command::Want {
        view: inbox_view,
        more: true,
    });
    let more = until(
        &rx,
        |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t[0].key.id == "t3"),
    );
    assert!(more.iter().any(|e| matches!(
        e,
        SyncEvent::ThreadPage { ids, mode: PageMode::Append, has_more: false, .. }
            if ids == &vec!["t3".to_string()]
    )));

    // Opening a thread fetches it in full.
    handle.send(Command::OpenThread(ThreadKey::new("me@example.com", A)));
    let opened = until(&rx, |e| matches!(e, SyncEvent::ThreadDetail(_)));
    let Some(SyncEvent::ThreadDetail(detail)) = opened.last() else {
        unreachable!()
    };
    assert_eq!(detail.key, ThreadKey::new("me@example.com", A));
    assert_eq!(detail.messages.len(), 2);
    events.extend(more);
    events.extend(opened);
    handle.send(Command::Shutdown);

    let reqs = server.finish();
    assert_eq!(reqs[0].path(), "/token");
    assert!(
        reqs[0]
            .form()
            .contains(&("refresh_token".into(), "rt".into()))
    );
    assert_eq!(reqs[1].path(), "/gmail/profile");
    assert!(
        reqs[1..]
            .iter()
            .filter(|r| r.path() != "/token")
            .all(|r| r.header("authorization").as_deref() == Some("Bearer at2")),
        "one refresh serves every call"
    );
    assert_eq!(reqs.iter().filter(|r| r.path() == "/token").count(), 1);
    assert_eq!(
        reqs.iter().filter(|r| r.path() == "/gmail/profile").count(),
        2
    );

    let lists: Vec<_> = reqs
        .iter()
        .filter(|r| r.path() == "/gmail/threads")
        .map(query)
        .collect();
    assert_eq!(lists.len(), 2);
    assert!(has(&lists[0], "labelIds", "INBOX") && has(&lists[0], "maxResults", "2"));
    assert!(!lists[0].iter().any(|(k, _)| k == "pageToken"));
    assert!(has(&lists[1], "pageToken", "09876543210"));

    let thread_reqs = |id: &str| -> Vec<Vec<(String, String)>> {
        reqs.iter()
            .filter(|r| r.path() == format!("/gmail/threads/{id}"))
            .map(query)
            .collect()
    };
    let b = thread_reqs(B);
    assert_eq!(b.len(), 2, "the deferred chunk ran again");
    assert!(
        b.iter()
            .all(|q| has(q, "format", "metadata") && has(q, "metadataHeaders", "Subject"))
    );
    let a = thread_reqs(A);
    assert_eq!(a.len(), 2);
    assert!(has(&a[0], "format", "metadata"));
    assert!(has(&a[1], "format", "full"));

    let counted: Vec<&str> = reqs
        .iter()
        .filter_map(|r| r.path().strip_prefix("/gmail/labels/"))
        .collect();
    for shown in ["INBOX", "DRAFT", "SPAM", "Label_1"] {
        assert!(counted.contains(&shown), "{shown} counted");
    }
    for hidden in ["UNREAD", "CHAT", "CATEGORY_PROMOTIONS", "Label_3"] {
        assert!(!counted.contains(&hidden), "{hidden} not counted");
    }
}

#[test]
fn sign_in_names_the_account_from_profile_and_saves_its_token() {
    let server = FakeServer::start(vec![Reply::json(200, PROFILE).for_path("/gmail/profile")]);
    let secrets = Secrets::memory();
    let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
    let seq = signing_in(&mut w);
    // A stale result is dropped.
    w.finish_sign_in(seq + 7, Ok(tokens(None)));
    assert!(drain(&rx).is_empty());

    w.finish_sign_in(seq, Ok(tokens(Some(oauth::GMAIL_MODIFY))));
    let events = drain(&rx);
    assert!(events.iter().any(|e| matches!(
        e,
        SyncEvent::Accounts(a) if a == &vec!["me@example.com".to_string()]
    )));
    assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
    let reqs = server.finish();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].header("authorization").as_deref(),
        Some("Bearer at")
    );
}

#[test]
fn missing_gmail_scope_saves_nothing_and_revokes_only_an_unknown_grant() {
    // No accounts yet: the new grant cannot belong to a known account, so it is revoked.
    let server = FakeServer::start(vec![Reply::json(200, "{}").for_path("/revoke")]);
    let secrets = Secrets::memory();
    let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
    let seq = signing_in(&mut w);
    w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
    assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
    assert!(secrets.tokens().unwrap().is_empty());
    assert!(w.accounts.is_empty());
    let reqs = server.finish();
    assert_eq!(reqs.len(), 1, "no profile call, one revoke");
    assert!(reqs[0].form().contains(&("token".into(), "rt".into())));

    // With an account here the grant might be its own: nothing is revoked.
    let (mut w, rx) = worker(secrets.clone(), nowhere());
    w.accounts.insert(
        "old@example.com".into(),
        AccountState::new("old".into(), None),
    );
    let seq = signing_in(&mut w);
    w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
    assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
    assert_eq!(w.accounts.len(), 1);
}

#[test]
fn invalid_grant_signs_the_account_out_with_a_hint() {
    let server = FakeServer::start(vec![Reply::json(
        400,
        r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
    )]);
    let secrets = Secrets::memory();
    secrets.set_token("me@example.com", "rt").unwrap();
    let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), None),
    );
    assert!(matches!(
        w.access_token("me@example.com"),
        Err(CallError::Auth)
    ));
    assert!(w.accounts.is_empty());
    assert!(secrets.tokens().unwrap().is_empty());
    assert_eq!(
        problem(&drain(&rx)).unwrap(),
        "Google signed me@example.com out. Add it again. (Test-mode tokens expire after 7 days.)"
    );
    assert_eq!(server.finish().len(), 1, "a local sign-out revokes nothing");
}

#[test]
fn signed_out_hint_depends_on_reauth() {
    assert!(
        signed_out_message("a@b", "reauth related error (invalid_rapt)")
            .ends_with("(Your organisation requires you to sign in again.)")
    );
    assert!(
        signed_out_message("a@b", "Bad Request")
            .ends_with("(Test-mode tokens expire after 7 days.)")
    );
}

#[test]
fn token_endpoint_outages_are_network_problems_that_keep_the_account() {
    let server = FakeServer::start(vec![Reply::json(503, "down"), Reply::json(200, TOKEN)]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), None),
    );
    assert!(matches!(
        w.access_token("me@example.com"),
        Err(CallError::Network)
    ));
    assert_eq!(w.accounts.len(), 1);
    assert_eq!(problem(&drain(&rx)).as_deref(), Some(NETWORK_PROBLEM));
    // The next success clears it.
    assert_eq!(
        w.access_token("me@example.com").ok().as_deref(),
        Some("at2")
    );
    assert_eq!(problem(&drain(&rx)).as_deref(), Some(""));
}

#[test]
fn a_rejected_access_token_is_refreshed_once() {
    let server = FakeServer::start(vec![
        Reply::json(401, "{}").for_path("/gmail/profile"),
        Reply::json(200, TOKEN).for_path("/token"),
        Reply::json(200, PROFILE).for_path("/gmail/profile"),
    ]);
    let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new(
            "rt".into(),
            Some(Access {
                token: "stale".into(),
                expires: Instant::now() + Duration::from_secs(3000),
            }),
        ),
    );
    let profile = w.with_token("me@example.com", gmail::profile).ok().unwrap();
    assert_eq!(profile.email_address, "me@example.com");
    let reqs = server.finish();
    assert_eq!(
        reqs[0].header("authorization").as_deref(),
        Some("Bearer stale")
    );
    assert_eq!(
        reqs[2].header("authorization").as_deref(),
        Some("Bearer at2")
    );
}

#[test]
fn saving_the_client_in_use_keeps_its_accounts() {
    let secrets = Secrets::memory();
    let (mut w, rx) = worker(secrets.clone(), nowhere());
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), None),
    );
    secrets.set_token("me@example.com", "rt").unwrap();
    // The built-in client's id with a secret pasted: same client, so nobody is signed out.
    w.save_client(client().client_id, Some("cs".into()));
    assert_eq!(secrets.client().unwrap(), Some(client()));
    assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
    assert_eq!(w.accounts.len(), 1);
    assert!(
        drain(&rx)
            .iter()
            .any(|e| matches!(e, SyncEvent::Client(Some(_))))
    );
}

fn signed_in(w: &mut Worker, email: &str, rt: &str) {
    w.accounts
        .insert(email.into(), AccountState::new(rt.into(), None));
}

#[test]
fn an_unexpected_refresh_error_is_reported_and_keeps_the_account() {
    let server = FakeServer::start(vec![
        Reply::json(
            400,
            r#"{"error":"invalid_request","error_description":"Missing parameter"}"#,
        ),
        Reply::json(200, TOKEN),
    ]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    assert!(matches!(
        w.access_token("me@example.com"),
        Err(CallError::Auth)
    ));
    assert_eq!(w.accounts.len(), 1);
    assert_eq!(
        problem(&drain(&rx)).as_deref(),
        Some("me@example.com: Missing parameter")
    );
    // A later success clears it through `check_accounts`' usual path.
    assert!(w.access_token("me@example.com").is_ok());
}

#[test]
fn an_unreadable_token_answer_is_reported_without_its_body() {
    let server = FakeServer::start(vec![Reply::json(
        200,
        r#"{"access_token":"ya29.secret","expires_in":"soon"}"#,
    )]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    assert!(w.access_token("me@example.com").is_err());
    let p = problem(&drain(&rx)).unwrap();
    assert!(
        p.starts_with("me@example.com: Google's sign-in service"),
        "{p}"
    );
    assert!(!p.contains("ya29") && !p.contains("soon"), "{p}");
}

#[test]
fn unreadable_tokens_are_reset_without_quoting_them() {
    let store = Arc::new(crate::secrets::MemoryStore::default());
    store
        .set(crate::secrets::TOKENS_USER, r#""1//0g-refresh""#)
        .unwrap();
    let (handle, rx) = start_with(
        Secrets::new(store.clone()),
        nowhere(),
        Some(client()),
        fast(),
        None,
        None,
    )
    .unwrap();
    let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
    let p = problem(&events).unwrap();
    assert!(p.starts_with("Stored sign-ins were unreadable"), "{p}");
    assert!(!p.contains("0g-refresh"), "{p}");
    assert_eq!(store.get(crate::secrets::TOKENS_USER).unwrap(), None);
    handle.send(Command::Shutdown);
}

#[test]
fn an_unreadable_client_is_removed_and_points_to_settings() {
    for builtin in [Some(client()), None] {
        let store = Arc::new(crate::secrets::MemoryStore::default());
        store
            .set(crate::secrets::CLIENT_USER, r#"{"client_id":"x"}"#)
            .unwrap();
        store
            .set(crate::secrets::TOKENS_USER, r#"{"me@example.com":"rt"}"#)
            .unwrap();
        let expect = if builtin.is_some() {
            AuthState::Ready
        } else {
            AuthState::NeedsClient
        };
        let (handle, rx) = start_with(
            Secrets::new(store.clone()),
            nowhere(),
            builtin,
            fast(),
            None,
            None,
        )
        .unwrap();
        let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SyncEvent::Auth(a) if *a == expect))
        );
        assert!(
            problem(&events)
                .unwrap()
                .contains("set it again in Settings")
        );
        assert_eq!(store.get(crate::secrets::CLIENT_USER).unwrap(), None);
        assert_eq!(store.get(crate::secrets::TOKENS_USER).unwrap(), None);
        handle.send(Command::Shutdown);
    }
}

/// A memory store whose client entry cannot be written.
#[derive(Default)]
struct ClientWriteFails(crate::secrets::MemoryStore);

impl SecretStore for ClientWriteFails {
    fn get(&self, user: &str) -> Result<Option<String>, SecretError> {
        self.0.get(user)
    }
    fn set(&self, user: &str, value: &str) -> Result<(), SecretError> {
        if user == crate::secrets::CLIENT_USER {
            return Err(SecretError::Unavailable("locked".into()));
        }
        self.0.set(user, value)
    }
    fn delete(&self, user: &str) -> Result<(), SecretError> {
        self.0.delete(user)
    }
}

#[test]
fn a_failed_client_save_keeps_the_accounts() {
    let store = Arc::new(ClientWriteFails::default());
    let secrets = Secrets::new(store.clone());
    secrets.set_token("me@example.com", "rt").unwrap();
    let (mut w, rx) = worker(secrets.clone(), nowhere());
    signed_in(&mut w, "me@example.com", "rt");
    w.handle(Command::SaveClient {
        client_id: "other.apps.googleusercontent.com".into(),
        client_secret: Some("s".into()),
    });
    let events = drain(&rx);
    assert!(problem(&events).unwrap().contains("locked"));
    // Nothing was signed out or revoked.
    assert!(!events.iter().any(|e| matches!(e, SyncEvent::Notice(_))));
    assert_eq!(w.accounts.len(), 1);
    assert_eq!(w.client, Some(client()));
    assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
}

fn outgoing(account: &str, thread_id: Option<&str>) -> Outgoing {
    Outgoing {
        account: account.into(),
        kind: crate::model::ComposeKind::Reply,
        from: crate::model::Address::new(Some("Me"), account),
        to: vec![crate::model::Address::new(Some("Zoë"), "zoe@example.org")],
        cc: vec![],
        bcc: vec![crate::model::Address::new(None, "hidden@example.org")],
        subject: "Re: Café plans".into(),
        body: "See you at noon.\n".into(),
        thread_id: thread_id.map(str::to_string),
        in_reply_to: Some("<m2@mail.example>".into()),
        references: vec!["<m2@mail.example>".into()],
        html: None,
        attachments: vec![],
    }
}

#[test]
fn drafts_are_saved_updated_deleted_and_read_back() {
    let draft = |id: &str| format!(r#"{{"id":"{id}","message":{{"id":"m9","threadId":"t9"}}}}"#);
    let message = r#"{"id":"m9","threadId":"t9","labelIds":["DRAFT"],"payload":{"mimeType":"text/plain",
        "headers":[{"name":"From","value":"Me <me@example.com>"},{"name":"To","value":"zoe@example.org"},
        {"name":"Subject","value":"Plans"},{"name":"In-Reply-To","value":"<m2@mail.example>"}],
        "body":{"size":5,"data":"SGVsbG8"}}}"#;
    let server = FakeServer::start(vec![
        Reply::json(200, TOKEN).for_path("/token").sticky(),
        Reply::json(200, &draft("r1")).for_path("/upload/drafts"),
        Reply::json(200, &draft("r1")).for_path("/upload/drafts/r1"),
        Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
            .for_path("/upload/drafts/gone"),
        Reply::json(200, &draft("r2")).for_path("/upload/drafts"),
        Reply::json(200, "").for_path("/gmail/drafts/r1"),
        Reply::json(200, &format!(r#"{{"drafts":[{}]}}"#, draft("r1"))).for_path("/gmail/drafts"),
        Reply::json(200, message).for_path("/gmail/messages/m9"),
    ]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    // No recipient yet is fine for a draft.
    let mut out = outgoing("me@example.com", None);
    out.to.clear();
    out.bcc.clear();
    let save = |op, draft_id: Option<&str>| DraftWork::Save {
        op,
        outgoing: Box::new(out.clone()),
        draft_id: draft_id.map(str::to_string),
    };
    w.draft(save(1, None));
    w.draft(save(2, Some("r1")));
    // A draft deleted elsewhere is created again.
    w.draft(save(3, Some("gone")));
    w.draft(DraftWork::Delete {
        account: "me@example.com".into(),
        draft_id: "r1".into(),
    });
    w.draft(DraftWork::Load {
        op: 4,
        account: "me@example.com".into(),
        thread_id: "t9".into(),
    });
    let events = drain(&rx);
    let saved: Vec<(OpId, String)> = events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::DraftSaved { op, draft_id } => Some((*op, draft_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        saved,
        [(1, "r1".to_string()), (2, "r1".into()), (3, "r2".into())]
    );
    let loaded = events
        .iter()
        .find_map(|e| match e {
            SyncEvent::DraftLoaded { op: 4, result } => Some(result.clone()),
            _ => None,
        })
        .expect("the draft is read back");
    let (id, back) = loaded.unwrap();
    assert_eq!(id, "r1");
    assert_eq!(
        (back.subject.as_str(), back.body.as_str(), back.to.len()),
        ("Plans", "Hello", 1)
    );
    assert_eq!(back.thread_id.as_deref(), Some("t9"));
    assert_eq!(back.kind, crate::model::ComposeKind::Reply);
    let requests = server.finish();
    let calls: Vec<(String, String)> = requests
        .iter()
        .filter(|r| r.path() != "/token")
        .map(|r| (r.method.clone(), r.path().to_string()))
        .collect();
    assert_eq!(
        calls,
        [
            ("POST", "/upload/drafts"),
            ("PUT", "/upload/drafts/r1"),
            ("PUT", "/upload/drafts/gone"),
            ("POST", "/upload/drafts"),
            ("DELETE", "/gmail/drafts/r1"),
            ("GET", "/gmail/drafts"),
            ("GET", "/gmail/messages/m9"),
        ]
        .map(|(m, p)| (m.to_string(), p.to_string()))
    );
}

#[test]
fn a_draft_with_html_and_an_attachment_is_read_back_whole() {
    let draft = r#"{"id":"r1","message":{"id":"m9","threadId":"t9"}}"#;
    // "<p>Hi <b>you</b></p>" and "PDF!" in base64url.
    let message = r#"{"id":"m9","threadId":"t9","labelIds":["DRAFT"],"payload":{"mimeType":"multipart/mixed",
        "headers":[{"name":"From","value":"Me <me@example.com>"},{"name":"To","value":"zoe@example.org"},
        {"name":"Subject","value":"Plans"}],
        "parts":[
          {"partId":"0","mimeType":"text/html","filename":"","body":{"size":20,"data":"PHA-SGkgPGI-eW91PC9iPjwvcD4"}},
          {"partId":"1","mimeType":"application/pdf","filename":"plan.pdf","body":{"size":4,"data":"UERGIQ"}}
        ]}}"#;
    let server = FakeServer::start(vec![
        Reply::json(200, TOKEN).for_path("/token").sticky(),
        Reply::json(200, &format!(r#"{{"drafts":[{draft}]}}"#)).for_path("/gmail/drafts"),
        // Read once for the draft and once more for the attachment's bytes.
        Reply::json(200, message).for_path("/gmail/messages/m9"),
        Reply::json(200, message).for_path("/gmail/messages/m9"),
    ]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    w.draft(DraftWork::Load {
        op: 1,
        account: "me@example.com".into(),
        thread_id: "t9".into(),
    });
    let events = drain(&rx);
    let (id, back) = events
        .iter()
        .find_map(|e| match e {
            SyncEvent::DraftLoaded { op: 1, result } => Some(result.clone()),
            _ => None,
        })
        .expect("the draft is read back")
        .unwrap();
    assert_eq!(id, "r1");
    assert!(
        back.html
            .as_deref()
            .is_some_and(|h| h.contains("<b>you</b>"))
    );
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(back.attachments[0].filename, "plan.pdf");
    match &back.attachments[0].source {
        AttachmentSource::Held(bytes) => assert_eq!(&bytes[..], b"PDF!"),
        other => panic!("held in memory, not {other:?}"),
    }
    drop(server.finish());
}

fn history_page(json: &str) -> gmail::HistoryList {
    serde_json::from_str(json).unwrap()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn collect_changes_folds_every_kind_and_filters_new_inbox_mail() {
    let fixture = history_page(fixtures::HISTORY);
    let added = |id: &str, labels: &[&str]| {
        format!(
            r#"{{"messagesAdded":[{{"message":{{"id":"x","threadId":"{id}","labelIds":{}}}}}]}}"#,
            serde_json::to_string(labels).unwrap()
        )
    };
    let second = history_page(&format!(
        r#"{{"history":[{},{},{},{},{}],"historyId":"123600"}}"#,
        added("promo", &["INBOX", "UNREAD", "CATEGORY_PROMOTIONS"]),
        added("social", &["INBOX", "UNREAD", "CATEGORY_SOCIAL"]),
        added("self", &["INBOX", "UNREAD", "SENT"]),
        added("read", &["INBOX"]),
        added("personal", &["INBOX", "UNREAD", "CATEGORY_PERSONAL"]),
    ));
    let c = collect_changes(&[fixture, second]);
    assert_eq!(
        c.changed,
        set(&[
            "t-new",
            "18c0a1b2c3d4e5f6",
            "t-old",
            "promo",
            "social",
            "self",
            "read",
            "personal"
        ]),
        "added, relabelled and deleted threads are all refetched"
    );
    assert_eq!(c.fresh_inbox, set(&["t-new", "personal"]));
    assert_eq!(c.labels, set(&["UNREAD"]));
    assert_eq!(c.new_history_id, Some(123600), "the last page's id");

    let empty = collect_changes(&[history_page(fixtures::HISTORY_EMPTY)]);
    assert!(empty.changed.is_empty() && empty.fresh_inbox.is_empty());
    assert_eq!(empty.new_history_id, Some(123456));
    assert_eq!(collect_changes(&[]).new_history_id, None);
}

fn summary(id: &str, label_ids: &[&str]) -> ThreadSummary {
    ThreadSummary {
        key: ThreadKey::new("me@example.com", id),
        history_id: 1,
        subject: String::new(),
        snippet: String::new(),
        participants: vec![],
        last_date: Timestamp::UNIX_EPOCH,
        message_count: 1,
        label_ids: label_ids.iter().map(|s| s.to_string()).collect(),
        has_attachments: false,
    }
}

fn know(account: &mut AccountState, id: &str, label_ids: &[&str]) {
    account.known.insert(
        id.into(),
        Known {
            history_id: 1,
            star_target: None,
            summary: summary(id, label_ids),
        },
    );
}

#[test]
fn the_cursor_moves_with_its_pending_work_and_failures_keep_it() {
    let mut a = AccountState::new("rt".into(), None);
    a.history_id = Some(5);
    a.commit_round(Changes {
        changed: set(&["a", "b", "c"]),
        fresh_inbox: set(&["c"]),
        labels: set(&["UNREAD"]),
        new_history_id: Some(9),
    });
    assert_eq!(a.history_id, Some(9), "committed with the pending ids");
    // New inbox mail first.
    assert_eq!(a.next_refetch(), ["c", "a", "b"]);
    // `c` lands, `a` is gone, then `b` fails: `b` stays pending for the next round.
    know(&mut a, "c", &["INBOX", "UNREAD"]);
    a.refetch_succeeded("c");
    a.refetch_gone("a");
    assert_eq!(a.pending_changed, set(&["b"]));
    assert_eq!(a.pending_fresh, set(&["c"]), "reported once the round ends");

    // A failed history page commits nothing; an empty round keeps what is pending.
    a.commit_round(Changes {
        new_history_id: Some(11),
        ..Changes::default()
    });
    assert_eq!(a.history_id, Some(11));
    assert_eq!(a.next_refetch(), ["b"]);
    know(&mut a, "b", &["INBOX"]);
    a.refetch_succeeded("b");
    assert!(a.pending_changed.is_empty());
    let new_mail = a.take_new_mail();
    assert_eq!(new_mail.len(), 1);
    assert_eq!(new_mail[0].key.id, "c");
    assert!(a.pending_fresh.is_empty());

    // A candidate that was read meanwhile is cleared without a report.
    a.commit_round(Changes {
        changed: set(&["d"]),
        fresh_inbox: set(&["d"]),
        ..Changes::default()
    });
    know(&mut a, "d", &["INBOX"]);
    a.refetch_succeeded("d");
    assert!(a.take_new_mail().is_empty());
    assert!(a.pending_fresh.is_empty());

    // An expired history drops every cache.
    a.commit_round(Changes {
        changed: set(&["e"]),
        ..Changes::default()
    });
    a.missing.insert("f".into(), 1);
    a.synced_once = true;
    a.reset();
    assert!(a.known.is_empty() && a.pending_changed.is_empty() && a.missing.is_empty());
    assert_eq!((a.history_id, a.synced_once), (None, false));
}

#[test]
fn a_burst_still_refetches_new_inbox_mail() {
    let mut a = AccountState::new("rt".into(), None);
    let ids: Vec<String> = (0..150).map(|i| format!("18c0{i:012x}")).collect();
    for id in &ids {
        know(&mut a, id, &["INBOX"]);
    }
    let fresh: Vec<&str> = ids[100..130].iter().map(String::as_str).collect();
    a.commit_round(Changes {
        changed: ids.iter().cloned().collect(),
        fresh_inbox: set(&fresh),
        ..Changes::default()
    });
    let first = a.next_refetch();
    assert_eq!(a.pending_changed.len(), BURST_FRESH);
    assert_eq!(a.pending_fresh, a.pending_changed);
    // The newest twenty new threads survive; the rest leave the cache.
    let kept: BTreeSet<String> = ids[110..130].iter().cloned().collect();
    assert_eq!(a.pending_changed, kept);
    assert_eq!(first.len(), CHUNK);
    assert!(first.iter().all(|id| kept.contains(id)));
    assert_eq!(a.known.len(), BURST_FRESH);
    assert!(!a.known.contains_key(&ids[0]));
}

#[test]
fn send_errors_say_whether_the_message_may_have_gone() {
    let maybe = |e: ApiError| send_error(&CallError::Api(e)).maybe_sent;
    assert!(maybe(ApiError::Network(api::NetKind::Timeout)));
    assert!(maybe(ApiError::Network(api::NetKind::Io)));
    for status in [500, 502, 503, 504] {
        assert!(maybe(ApiError::Http {
            status,
            body: String::new()
        }));
    }
    assert!(
        maybe(ApiError::Decode("eof".into())),
        "a 2xx we could not read"
    );
    for e in [
        ApiError::Http {
            status: 400,
            body: "Invalid To header".into(),
        },
        ApiError::Http {
            status: 413,
            body: String::new(),
        },
        ApiError::QuotaExhausted {
            message: "sending limit".into(),
            retry_after: None,
        },
        ApiError::Unauthorized,
        ApiError::Forbidden("no".into()),
        ApiError::RateLimited { retry_after: None },
    ] {
        assert!(!maybe(e.clone()), "{e:?}");
    }
    let e = send_error(&CallError::Api(ApiError::Http {
        status: 400,
        body: "Invalid To header".into(),
    }));
    assert!(e.message.contains("Invalid To header"));
    assert_eq!(
        send_error(&CallError::Api(ApiError::Network(api::NetKind::Connect))).message,
        MAYBE_SENT
    );
    // The token could not be refreshed, so nothing went out.
    assert!(!send_error(&CallError::Network).maybe_sent);
    assert!(!send_error(&CallError::Auth).maybe_sent);

    // A Gmail-held attachment that cannot be fetched stops the send before any upload.
    let mut out = outgoing("me@example.com", None);
    out.attachments.push(OutgoingAttachment {
        filename: "a\u{202E}fdp.exe".into(),
        mime_type: "application/pdf".into(),
        size: 1,
        source: AttachmentSource::Gmail {
            message_id: "m".into(),
            part_id: "1".into(),
        },
    });
    let e = gather_attachments(&out, |_, _| Err(ATTACHMENT_GONE.into()))
        .err()
        .unwrap();
    assert_eq!(
        e.message,
        format!("Couldn't attach afdp.exe: {ATTACHMENT_GONE}")
    );
    assert!(!e.maybe_sent);
    // Its declared size counts against the 25 MB before anything is downloaded.
    out.attachments[0].size = MAX_OUTGOING_ATTACHMENTS + 1;
    let e = gather_attachments(&out, |_, _| panic!("not downloaded"))
        .err()
        .unwrap();
    assert_eq!(e.message, compose::TOO_LARGE_TO_SEND);
    // The fetched bytes are used, with their real size.
    out.attachments[0].size = 1;
    let parts = gather_attachments(&out, |m, p| {
        assert_eq!((m, p), ("m", "1"));
        Ok(b"%PDF".to_vec())
    })
    .unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].0.size, 4);
    assert_eq!(parts[0].1, b"%PDF");
}

#[test]
fn auth_network_errors_take_the_backoff_path() {
    let server = FakeServer::start(vec![Reply::json(503, "down").for_path("/token")]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    w.fetch_meta("me@example.com", vec!["t1".into(), "t2".into()]);
    let account = &w.accounts["me@example.com"];
    assert!(account.backoff.waiting(Instant::now()), "backed off");
    assert_eq!(account.backoff.attempts, 1);
    assert_eq!(
        account.deferred,
        [Job::FetchMeta(
            "me@example.com".into(),
            vec!["t1".into(), "t2".into()]
        )],
        "the chunk is parked, not lost"
    );
    assert_eq!(w.accounts.len(), 1, "the account stays");
    assert_eq!(problem(&drain(&rx)).as_deref(), Some(NETWORK_PROBLEM));
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn a_failed_restore_after_untrash_still_refetches_the_thread() {
    let server = FakeServer::start(vec![
        Reply::json(200, TOKEN).for_path("/token"),
        Reply::json(200, r#"{"id":"t1"}"#).for_path("/gmail/threads/t1/untrash"),
        Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
            .for_path("/gmail/threads/t1/modify"),
    ]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    w.mutate(Mutation {
        op: 9,
        account: "me@example.com".into(),
        thread_ids: vec!["t1".into(), "t2".into()],
        change: Change::Untrash(LabelDelta::archive().inverse()),
    });
    let failed = drain(&rx).into_iter().any(|e| {
        matches!(
            e,
            SyncEvent::MutationFinished {
                op: 9,
                error: Some(_)
            }
        )
    });
    assert!(failed, "the store rolls the change back");
    let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
    assert!(
        jobs.iter().any(|j| matches!(
            j,
            Job::FetchMeta(a, ids) if a == "me@example.com" && ids.iter().any(|i| i == "t1")
        )),
        "the half-applied untrash is corrected by a refetch: {jobs:?}"
    );
    let reqs = server.finish();
    assert!(
        reqs.iter().all(|r| !r.path().contains("/t2/")),
        "stops at the failure"
    );
}

#[test]
fn starring_a_thread_of_drafts_stars_the_thread() {
    let drafts = r#"{"id":"td","historyId":"5","messages":[
        {"id":"d1","threadId":"td","labelIds":["DRAFT"],"historyId":"5","internalDate":"1700000000000"}
    ]}"#;
    let server = FakeServer::start(vec![
        Reply::json(200, TOKEN).for_path("/token"),
        Reply::json(200, drafts).for_path("/gmail/threads/td"),
        Reply::json(200, "{}").for_path("/gmail/threads/td/modify"),
    ]);
    let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
    signed_in(&mut w, "me@example.com", "rt");
    w.mutate(Mutation {
        op: 3,
        account: "me@example.com".into(),
        thread_ids: vec!["td".into()],
        change: Change::Modify(LabelDelta::star()),
    });
    let ok = drain(&rx)
        .into_iter()
        .any(|e| matches!(e, SyncEvent::MutationFinished { op: 3, error: None }));
    assert!(ok, "no misleading 'no longer in Gmail'");
    let reqs = server.finish();
    let modify = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path() == "/gmail/threads/td/modify")
        .expect("the thread is starred");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&modify.body).unwrap(),
        serde_json::json!({"addLabelIds": ["STARRED"]})
    );
}

#[test]
fn a_retried_first_sync_takes_the_new_cursor() {
    for (synced_once, expected) in [(false, 123456), (true, 7)] {
        let server = FakeServer::start(vec![
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
            Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
                .for_path("/gmail/labels"),
        ]);
        let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        {
            let a = w.accounts.get_mut("me@example.com").unwrap();
            a.history_id = Some(7);
            a.synced_once = synced_once;
        }
        w.initial_sync("me@example.com");
        assert_eq!(
            w.accounts["me@example.com"].history_id,
            Some(expected),
            "synced_once = {synced_once}"
        );
        server.finish();
    }
}

/// Waits until an event in `all` (old or newly received) matches `done`.
fn wait_for(
    rx: &async_channel::Receiver<SyncEvent>,
    all: &mut Vec<SyncEvent>,
    done: impl Fn(&SyncEvent) -> bool,
) {
    if all.iter().any(&done) {
        return;
    }
    all.extend(until(rx, done));
}

/// The parts of a `multipart/related` body, as (content type, bytes).
fn multipart_parts(content_type: &str, body: &[u8]) -> Vec<(String, Vec<u8>)> {
    let boundary = content_type
        .split_once("boundary=")
        .map(|(_, b)| b.trim_matches('"'))
        .unwrap();
    let delimiter = format!("--{boundary}");
    let text = body.to_vec();
    let mut parts = Vec::new();
    let mut rest = &text[..];
    while let Some(start) = find(rest, delimiter.as_bytes()) {
        rest = &rest[start + delimiter.len()..];
        if rest.starts_with(b"--") {
            break;
        }
        let Some(end) = find(rest, delimiter.as_bytes()) else {
            break;
        };
        let part = &rest[..end];
        let part = part.strip_prefix(b"\r\n").unwrap_or(part);
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let split = find(part, b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&part[..split]).to_string();
        let ct = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Type: "))
            .unwrap_or_default()
            .to_string();
        parts.push((ct, part[split + 4..].to_vec()));
        rest = &rest[end..];
    }
    parts
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// SPEC 12.2 `fake_gmail_write_path`: changes, stars, trash and untrash, history rounds
/// with new mail, a failed refetch, a deleted thread, an expired history, a stale access
/// token, and sends, all through the real worker thread against a scripted Gmail.
#[test]
fn fake_gmail_write_path() {
    let labels: gmail::LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
    let listed = format!(r#"{{"threads":[{{"id":"{A}","historyId":"123450"}}]}}"#);
    let round_one = r#"{"history":[
        {"messagesAdded":[{"message":{"id":"m9","threadId":"tnew","labelIds":["INBOX","UNREAD"]}}]},
        {"messagesDeleted":[{"message":{"id":"m5","threadId":"t-old"}}]}
    ],"historyId":"200"}"#;
    let ok = "{}";
    // A message with an inline attachment (`body.data`) and one held by `attachmentId`.
    let pdf: &[u8] = b"%PDF-1.4\x00\xff\xfe binary";
    let message_full = format!(
        r#"{{"id":"mfull","threadId":"{A}","payload":{{"partId":"","mimeType":"multipart/mixed","parts":[
            {{"partId":"0","mimeType":"text/plain","body":{{"size":2,"data":"aGk"}}}},
            {{"partId":"1","mimeType":"text/plain","filename":"notes.txt","body":{{"size":11,"data":"aGVsbG8gd29ybGQ"}}}},
            {{"partId":"2","mimeType":"application/pdf","filename":"../report.pdf","body":{{"size":{},"attachmentId":"ATT9"}}}},
            {{"partId":"3","mimeType":"application/octet-stream","filename":"bad.bin","body":{{"size":3,"data":"!!!"}}}}
        ]}}}}"#,
        pdf.len()
    );
    let attachment_body = format!(
        r#"{{"size":{},"data":"{}"}}"#,
        pdf.len(),
        crate::mail::b64::encode(pdf)
    );
    let downloads =
        std::env::temp_dir().join(format!("eloquawk-sync-downloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&downloads);
    let mut script = vec![
        Reply::json(200, TOKEN).for_path("/token"),
        // The second history call finds the access token stale: one refresh, one retry.
        Reply::json(200, TOKEN).for_path("/token"),
        Reply::json(200, fixtures::PROFILE)
            .for_path("/gmail/profile")
            .sticky(),
        Reply::json(200, fixtures::LABELS)
            .for_path("/gmail/labels")
            .sticky(),
        Reply::json(200, &listed)
            .for_path("/gmail/threads")
            .sticky(),
        Reply::json(200, &meta(A))
            .for_path(&format!("/gmail/threads/{A}"))
            .sticky(),
        // Archive both threads, then star, trash and untrash with the inbox put back.
        Reply::json(200, ok).for_path(&format!("/gmail/threads/{A}/modify")),
        Reply::json(200, ok).for_path("/gmail/threads/t2/modify"),
        Reply::json(200, &meta("t2")).for_path("/gmail/threads/t2"),
        Reply::json(200, r#"{"id":"m2","threadId":"x","labelIds":["STARRED"]}"#)
            .for_path("/gmail/messages/m2/modify"),
        Reply::json(200, &format!(r#"{{"id":"{A}"}}"#))
            .for_path(&format!("/gmail/threads/{A}/trash")),
        Reply::json(200, &format!(r#"{{"id":"{A}"}}"#))
            .for_path(&format!("/gmail/threads/{A}/untrash")),
        Reply::json(200, ok).for_path(&format!("/gmail/threads/{A}/modify")),
        // Round one: new inbox mail and a deleted message; the new thread's refetch fails.
        Reply::json(200, round_one).for_path("/gmail/history"),
        Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
            .for_path("/gmail/threads/tnew"),
        // Round two: nothing new, after a stale token; the pending threads are refetched.
        Reply::json(401, r#"{"error":{"code":401,"message":"stale"}}"#).for_path("/gmail/history"),
        Reply::json(200, r#"{"historyId":"200"}"#).for_path("/gmail/history"),
        Reply::json(200, &meta("tnew")).for_path("/gmail/threads/tnew"),
        Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
            .for_path("/gmail/threads/t-old"),
        // Round three: the history expired.
        Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
            .for_path("/gmail/history"),
        Reply::json(200, r#"{"historyId":"123456"}"#)
            .for_path("/gmail/history")
            .sticky(),
        // Sends: a 503 (may have gone), then a success.
        Reply::json(503, r#"{"error":{"code":503,"message":"Unavailable"}}"#)
            .for_path("/upload/messages/send"),
        Reply::json(200, fixtures::SEND).for_path("/upload/messages/send"),
        // Saves and a forward read the message afresh, then the attachment by its id.
        Reply::json(200, &message_full)
            .for_path("/gmail/messages/mfull")
            .sticky(),
        Reply::json(200, &attachment_body)
            .for_path("/gmail/messages/mfull/attachments/ATT9")
            .sticky(),
        Reply::json(200, fixtures::SEND).for_path("/upload/messages/send"),
    ];
    for l in &labels.labels {
        script.push(
            Reply::json(
                200,
                &format!(
                    r#"{{"id":"{}","name":"{}","threadsUnread":1,"threadsTotal":2}}"#,
                    l.id, l.name
                ),
            )
            .for_path(&format!("/gmail/labels/{}", l.id))
            .sticky(),
        );
    }
    let server = FakeServer::start(script);
    let secrets = Secrets::memory();
    secrets.set_token("me@example.com", "rt").unwrap();
    let (handle, rx) = start_with(
        secrets,
        fake_endpoints(&server),
        Some(client()),
        fast(),
        Some(downloads.clone()),
        None,
    )
    .unwrap();
    let me = || "me@example.com".to_string();
    let mut all = Vec::new();

    // The first sync, which never reports new mail.
    wait_for(
        &rx,
        &mut all,
        |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t[0].key.id == A),
    );
    let finished = |op: OpId| move |e: &SyncEvent| matches!(e, SyncEvent::MutationFinished { op: o, .. } if *o == op);
    let mutation_error = |all: &[SyncEvent], op: OpId| {
        all.iter().find_map(|e| match e {
            SyncEvent::MutationFinished { op: o, error } if *o == op => Some(error.clone()),
            _ => None,
        })
    };

    handle.send(Command::Modify {
        op: 1,
        account: me(),
        thread_ids: vec![A.into(), "t2".into()],
        delta: LabelDelta::archive(),
    });
    wait_for(&rx, &mut all, finished(1));
    // The authoritative state follows the change.
    wait_for(
        &rx,
        &mut all,
        |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t.iter().any(|s| s.key.id == "t2")),
    );
    handle.send(Command::Modify {
        op: 2,
        account: me(),
        thread_ids: vec![A.into()],
        delta: LabelDelta::star(),
    });
    wait_for(&rx, &mut all, finished(2));
    handle.send(Command::Trash {
        op: 3,
        account: me(),
        thread_ids: vec![A.into()],
    });
    wait_for(&rx, &mut all, finished(3));
    handle.send(Command::Untrash {
        op: 4,
        account: me(),
        thread_ids: vec![A.into()],
        restore: LabelDelta::archive().inverse(),
    });
    wait_for(&rx, &mut all, finished(4));
    for op in 1..=4 {
        assert_eq!(mutation_error(&all, op), Some(None), "op {op}");
    }

    // History rounds: new mail is reported once its refetch finally works, and the deleted
    // thread is gone.
    wait_for(&rx, &mut all, |e| matches!(e, SyncEvent::NewMail { .. }));
    let new_mail: Vec<&Vec<ThreadSummary>> = all
        .iter()
        .filter_map(|e| match e {
            SyncEvent::NewMail { threads, .. } => Some(threads),
            _ => None,
        })
        .collect();
    assert_eq!(new_mail.len(), 1);
    assert_eq!(new_mail[0].len(), 1);
    assert_eq!(new_mail[0][0].key, ThreadKey::new("me@example.com", "tnew"));
    assert!(all.iter().any(|e| matches!(
        e,
        SyncEvent::ThreadsGone { ids, .. } if ids == &vec!["t-old".to_string()]
    )));
    let refresh_after_new_mail = all.iter().any(|e| {
        matches!(
            e,
            SyncEvent::ThreadPage {
                mode: PageMode::Refresh,
                ..
            }
        )
    });
    assert!(refresh_after_new_mail, "the round re-lists page 1");

    // The expired history resets the account and syncs it from scratch.
    wait_for(&rx, &mut all, |e| matches!(e, SyncEvent::AccountReset(_)));
    let reset_at = all
        .iter()
        .position(|e| matches!(e, SyncEvent::AccountReset(a) if a == "me@example.com"))
        .unwrap();
    let mut after = all.split_off(reset_at);
    wait_for(&rx, &mut after, |e| {
        matches!(
            e,
            SyncEvent::ThreadPage {
                mode: PageMode::Replace,
                ..
            }
        )
    });
    all.extend(after);

    // A send that may have gone out, then one that did.
    handle.send(Command::Send {
        op: 5,
        outgoing: outgoing("me@example.com", Some(A)),
    });
    wait_for(&rx, &mut all, |e| {
        matches!(e, SyncEvent::SendFinished { op: 5, .. })
    });
    handle.send(Command::Send {
        op: 6,
        outgoing: outgoing("me@example.com", Some(A)),
    });
    wait_for(&rx, &mut all, |e| {
        matches!(e, SyncEvent::SendFinished { op: 6, .. })
    });
    let send_result = |op: OpId| send_result_in(&all, op);
    assert_eq!(
        send_result(5),
        Some(Some(SendError {
            message: MAYBE_SENT.into(),
            maybe_sent: true
        }))
    );
    assert_eq!(send_result(6), Some(None));

    // Saves: an inline part, the same part again (never overwritten), and one fetched by
    // its attachment id under a sanitised name.
    let part = |part_id: &str, filename: &str, attachment_id: Option<&str>| Attachment {
        part_id: part_id.into(),
        // A stale id from an earlier fetch: the worker looks up a fresh one.
        attachment_id: attachment_id.map(str::to_string),
        filename: filename.into(),
        mime_type: "application/octet-stream".into(),
        size: 11,
    };
    let mut saved = Vec::new();
    for attachment in [
        part("1", "notes.txt", None),
        part("1", "notes.txt", None),
        part("2", "../report.pdf", Some("stale")),
    ] {
        handle.send(Command::SaveAttachment {
            account: me(),
            message_id: "mfull".into(),
            attachment,
        });
        let events = until(&rx, |e| {
            matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
        });
        match events.last() {
            Some(SyncEvent::Saved(path)) => saved.push(path.clone()),
            other => panic!("not saved: {other:?}"),
        }
        all.extend(events);
    }
    assert_eq!(
        saved,
        [
            downloads.join("notes.txt"),
            downloads.join("notes (1).txt"),
            downloads.join("report.pdf"),
        ]
    );
    assert_eq!(std::fs::read(&saved[0]).unwrap(), b"hello world");
    assert_eq!(std::fs::read(&saved[1]).unwrap(), b"hello world");
    assert_eq!(std::fs::read(&saved[2]).unwrap(), pdf);
    // A part that is no longer in the message is a toast, and nothing is written.
    handle.send(Command::SaveAttachment {
        account: me(),
        message_id: "mfull".into(),
        attachment: part("9", "gone.bin", None),
    });
    let events = until(&rx, |e| {
        matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
    });
    assert!(
        matches!(events.last(), Some(SyncEvent::Notice(n)) if n == &format!("Couldn't save gone.bin: {ATTACHMENT_GONE}")),
        "{events:?}"
    );
    all.extend(events);
    // Data that is not base64 is a toast too, without backing the account off: no banner.
    handle.send(Command::SaveAttachment {
        account: me(),
        message_id: "mfull".into(),
        attachment: part("3", "bad.bin", None),
    });
    let events = until(&rx, |e| {
        matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
    });
    assert!(
        matches!(events.last(), Some(SyncEvent::Notice(n)) if n == &format!("Couldn't save bad.bin: {ATTACHMENT_UNREADABLE}")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SyncEvent::Problem(Some(_)))),
        "{events:?}"
    );
    assert!(!downloads.join("bad.bin").exists());
    all.extend(events);
    let _ = std::fs::remove_dir_all(&downloads);

    // A forward carrying the original's attachment.
    let mut forward = outgoing("me@example.com", Some(A));
    forward.attachments.push(OutgoingAttachment {
        filename: "report.pdf".into(),
        mime_type: "application/pdf".into(),
        size: pdf.len() as u64,
        source: AttachmentSource::Gmail {
            message_id: "mfull".into(),
            part_id: "2".into(),
        },
    });
    handle.send(Command::Send {
        op: 7,
        outgoing: forward,
    });
    wait_for(&rx, &mut all, |e| {
        matches!(e, SyncEvent::SendFinished { op: 7, .. })
    });
    assert_eq!(send_result_in(&all, 7), Some(None));
    assert_eq!(
        all.iter()
            .filter(|e| matches!(e, SyncEvent::NewMail { .. }))
            .count(),
        1,
        "the sync after the reset reports nothing"
    );
    handle.send(Command::Shutdown);

    let reqs = server.finish();
    let posts = |path: &str| -> Vec<&crate::google::fake::Recorded> {
        reqs.iter()
            .filter(|r| r.method == "POST" && r.path() == path)
            .collect()
    };
    let body = |r: &crate::google::fake::Recorded| -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap()
    };
    let a_modify = posts(&format!("/gmail/threads/{A}/modify"));
    assert_eq!(a_modify.len(), 2);
    assert_eq!(
        body(a_modify[0]),
        serde_json::json!({"removeLabelIds": ["INBOX"]})
    );
    assert_eq!(
        body(a_modify[1]),
        serde_json::json!({"addLabelIds": ["INBOX"]}),
        "untrash puts the inbox back"
    );
    let t2 = posts("/gmail/threads/t2/modify");
    assert_eq!(t2.len(), 1, "one modify per thread");
    assert_eq!(
        body(t2[0]),
        serde_json::json!({"removeLabelIds": ["INBOX"]})
    );
    let star = posts("/gmail/messages/m2/modify");
    assert_eq!(star.len(), 1, "star goes on the last message");
    assert_eq!(
        body(star[0]),
        serde_json::json!({"addLabelIds": ["STARRED"]})
    );
    assert_eq!(posts(&format!("/gmail/threads/{A}/trash")).len(), 1);
    let untrash_at = reqs
        .iter()
        .position(|r| r.path() == format!("/gmail/threads/{A}/untrash"))
        .unwrap();
    assert_eq!(
        reqs[untrash_at + 1].path(),
        format!("/gmail/threads/{A}/modify"),
        "the restore follows the untrash"
    );

    // History: repeated types from the cursor, and one refresh for the stale token.
    let history: Vec<&crate::google::fake::Recorded> = reqs
        .iter()
        .filter(|r| r.path() == "/gmail/history")
        .collect();
    assert!(history.len() >= 4);
    let q = query(history[0]);
    assert!(has(&q, "startHistoryId", "123456"));
    assert_eq!(q.iter().filter(|(k, _)| k == "historyTypes").count(), 4);
    assert!(
        has(&query(history[1]), "startHistoryId", "200"),
        "cursor moved"
    );
    assert_eq!(reqs.iter().filter(|r| r.path() == "/token").count(), 2);
    let second_token = reqs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.path() == "/token")
        .nth(1)
        .unwrap()
        .0;
    assert_eq!(reqs[second_token - 1].path(), "/gmail/history");
    assert_eq!(reqs[second_token + 1].path(), "/gmail/history");
    assert_eq!(
        reqs[second_token + 1].header("authorization").as_deref(),
        Some("Bearer at2")
    );
    let tnew = reqs
        .iter()
        .filter(|r| r.path() == "/gmail/threads/tnew")
        .count();
    assert_eq!(tnew, 2, "the failed refetch was retried by the next round");
    // The reset's initial sync listed the inbox again.
    assert_eq!(
        reqs.iter().filter(|r| r.path() == "/gmail/profile").count(),
        2
    );

    // Saves and the forward: the message read afresh each time, the attachment by the
    // fresh id (never the stale one), and never for an inline part.
    let full: Vec<_> = reqs
        .iter()
        .filter(|r| r.path() == "/gmail/messages/mfull")
        .collect();
    assert_eq!(
        full.len(),
        6,
        "three saves, a missing part, a bad one and the forward"
    );
    assert!(full.iter().all(|r| has(&query(r), "format", "full")));
    assert_eq!(
        reqs.iter()
            .filter(|r| r.path() == "/gmail/messages/mfull/attachments/ATT9")
            .count(),
        2,
        "the pdf save and the forward"
    );
    assert!(!reqs.iter().any(|r| r.path().contains("stale")));

    // Sends: a multipart upload carrying the thread id and the raw message.
    let sends = posts("/upload/messages/send");
    assert_eq!(sends.len(), 3);
    for send in &sends {
        assert!(has(&query(send), "uploadType", "multipart"));
        let ct = send.header("content-type").unwrap();
        assert!(ct.starts_with("multipart/related; boundary="), "{ct}");
        let parts = multipart_parts(&ct, &send.body);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "application/json; charset=UTF-8");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&parts[0].1).unwrap(),
            serde_json::json!({"threadId": A})
        );
        assert_eq!(parts[1].0, "message/rfc822");
        let m = mail_parser::MessageParser::default()
            .parse(&parts[1].1[..])
            .unwrap();
        assert_eq!(m.subject(), Some("Re: Café plans"));
        assert_eq!(
            m.in_reply_to().and_then(|l| l.first()),
            Some("m2@mail.example")
        );
        let bcc = m.bcc().and_then(|l| l.first()).and_then(|a| a.address());
        assert_eq!(bcc, Some("hidden@example.org"), "Bcc stays for Gmail");
    }
    let forwarded = multipart_parts(&sends[2].header("content-type").unwrap(), &sends[2].body);
    let m = mail_parser::MessageParser::default()
        .parse(&forwarded[1].1[..])
        .unwrap();
    let parts: Vec<_> = m.attachments().collect();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].attachment_name(), Some("report.pdf"));
    assert!(parts[0].is_content_type("application", "pdf"));
    assert_eq!(&parts[0].decoded()[..], pdf, "the original bytes");
}

fn send_result_in(all: &[SyncEvent], op: OpId) -> Option<Option<SendError>> {
    all.iter().find_map(|e| match e {
        SyncEvent::SendFinished { op: o, error } if *o == op => Some(error.clone()),
        _ => None,
    })
}

/// The two runs of the relaunch tests: the first lists the inbox and opens thread `A`;
/// the second starts from what the first saved.
struct Relaunch {
    cache_dir: PathBuf,
    secrets: Secrets,
}

impl Relaunch {
    fn new(name: &str) -> Self {
        let cache_dir = std::env::temp_dir().join(format!(
            "eloquawk-sync-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_dir);
        // Secrets outlive both runs, as the keyring does, so the cache key is the same.
        let secrets = Secrets::memory();
        secrets.set_token("me@example.com", "rt").unwrap();
        Self { cache_dir, secrets }
    }

    /// Runs a worker against `script` until it settles, optionally opening `A` after,
    /// and returns every event and every request.
    fn run(
        &self,
        script: Vec<Reply>,
        open: bool,
    ) -> (Vec<SyncEvent>, Vec<crate::google::fake::Recorded>) {
        let server = FakeServer::start(script);
        let (handle, rx) = start_with(
            self.secrets.clone(),
            fake_endpoints(&server),
            Some(client()),
            fast(),
            None,
            Some(self.cache_dir.clone()),
        )
        .unwrap();
        let mut events = until(&rx, |e| matches!(e, SyncEvent::Synced(_)));
        events.extend(until(&rx, |e| matches!(e, SyncEvent::Busy(false))));
        if open {
            handle.send(Command::OpenThread(ThreadKey::new("me@example.com", A)));
            events.extend(until(&rx, |e| matches!(e, SyncEvent::ThreadDetail(_))));
        }
        handle.send(Command::Shutdown);
        while let Ok(ev) = rx.recv_blocking() {
            events.push(ev);
        }
        (events, server.finish())
    }

    /// Gmail as the first launch sees it: two threads in the inbox, and `A` opened.
    fn first_script() -> Vec<Reply> {
        let list = format!(
            r#"{{"threads":[{{"id":"{A}","historyId":"123450"}},{{"id":"{B}","historyId":"123450"}}]}}"#
        );
        let mut script = vec![
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
            Reply::json(200, fixtures::HISTORY_EMPTY)
                .for_path("/gmail/history")
                .sticky(),
            Reply::json(200, fixtures::LABELS).for_path("/gmail/labels"),
            Reply::json(200, &list).for_path("/gmail/threads"),
        ];
        for id in [A, B] {
            script.push(Reply::json(200, &meta(id)).for_path(&format!("/gmail/threads/{id}")));
        }
        script
            .push(Reply::json(200, fixtures::THREAD_FULL).for_path(&format!("/gmail/threads/{A}")));
        script.extend(Self::counts());
        script
    }

    /// A launch that only follows the history: no profile, no listing.
    fn resume_script() -> Vec<Reply> {
        let mut script = vec![
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, fixtures::HISTORY_EMPTY)
                .for_path("/gmail/history")
                .sticky(),
            Reply::json(200, fixtures::LABELS).for_path("/gmail/labels"),
        ];
        script.extend(Self::counts());
        script
    }

    fn counts() -> Vec<Reply> {
        let labels: gmail::LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
        labels
            .labels
            .iter()
            .map(|l| {
                Reply::json(200, fixtures::LABEL_INBOX.replace("INBOX", &l.id).as_str())
                    .for_path(&format!("/gmail/labels/{}", l.id))
                    .sticky()
            })
            .collect()
    }
}

impl Drop for Relaunch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

fn count(requests: &[crate::google::fake::Recorded], path: &str) -> usize {
    requests.iter().filter(|r| r.path() == path).count()
}

/// SPEC 7.7 and 7.8: a second launch shows the cached labels and inbox before any request,
/// then follows the history from the saved cursor instead of listing and fetching again,
/// and a thread opened before is read from disk.
#[test]
fn a_relaunch_resumes_from_the_saved_cursor() {
    let r = Relaunch::new("resume");
    let (_, requests) = r.run(Relaunch::first_script(), true);
    assert_eq!(count(&requests, &format!("/gmail/threads/{A}")), 2);

    let (events, requests) = r.run(Relaunch::resume_script(), true);
    let first_request = events
        .iter()
        .position(|e| matches!(e, SyncEvent::Busy(true)))
        .unwrap();
    let cached = events[..first_request]
        .iter()
        .find_map(|e| match e {
            SyncEvent::ThreadPage {
                ids,
                threads,
                mode: PageMode::Refresh,
                ..
            } => Some((ids.clone(), threads.len())),
            _ => None,
        })
        .expect("the cached inbox comes before any request");
    assert_eq!(cached, (vec![A.to_string(), B.to_string()], 2));
    assert!(
        events[..first_request]
            .iter()
            .any(|e| matches!(e, SyncEvent::Labels { labels, .. } if !labels.is_empty()))
    );
    // The cursor the first launch ended at is where the history is read from.
    let history: Vec<_> = requests
        .iter()
        .filter(|r| r.path() == "/gmail/history")
        .collect();
    assert!(!history.is_empty());
    assert!(has(&query(history[0]), "startHistoryId", "123456"));
    assert_eq!(count(&requests, "/gmail/profile"), 0, "no initial sync");
    assert_eq!(
        count(&requests, "/gmail/threads"),
        0,
        "nothing is listed again"
    );
    assert_eq!(
        count(&requests, "/gmail/labels"),
        1,
        "the label list is refreshed once"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.path().starts_with("/gmail/threads/")),
        "no thread is fetched, the opened one comes from disk"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SyncEvent::NewMail { .. })),
        "nothing is announced as new on resuming"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        SyncEvent::ThreadDetail(d) if d.key.id == A && d.messages.len() == 2
    )));
}

/// Gmail no longer has the saved cursor: the account resets and lists afresh.
#[test]
fn a_resume_with_an_expired_cursor_falls_back_to_an_initial_sync() {
    let r = Relaunch::new("expired");
    r.run(Relaunch::first_script(), true);
    let mut script = vec![
        Reply::json(200, TOKEN).for_path("/token"),
        Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
            .for_path("/gmail/history"),
        Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
        Reply::json(200, fixtures::LABELS).for_path("/gmail/labels"),
        Reply::json(200, r#"{"threads":[]}"#).for_path("/gmail/threads"),
    ];
    script.extend(Relaunch::counts());
    let (events, requests) = r.run(script, false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SyncEvent::AccountReset(_)))
    );
    assert_eq!(count(&requests, "/gmail/profile"), 1);
    assert_eq!(count(&requests, "/gmail/threads"), 1);
    // Whatever was saved for the thread is gone with the reset.
    let threads = r.cache_dir.join("threads");
    let left = std::fs::read_dir(threads)
        .map(|d| {
            d.flatten()
                .flat_map(|a| std::fs::read_dir(a.path()).unwrap())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(left, 0);
}

/// A cursor saved with changes still pending would skip them: none is saved.
#[test]
fn a_snapshot_taken_mid_round_carries_no_cursor() {
    let mut a = AccountState::new("rt".into(), None);
    a.history_id = Some(9);
    a.synced_once = true;
    assert_eq!(a.snapshot().history_id, Some(9));
    a.pending_changed.insert("t".into());
    assert_eq!(a.snapshot().history_id, None);
    a.pending_changed.clear();
    a.synced_once = false;
    assert_eq!(
        a.snapshot().history_id,
        None,
        "an unfinished first sync neither"
    );
    // Restoring without a cursor leaves the account to sync from scratch.
    let mut b = AccountState::new("rt".into(), None);
    let snap = Snapshot {
        labels: vec![Label {
            account: "me@example.com".into(),
            id: "INBOX".into(),
            name: "INBOX".into(),
            kind: LabelKind::System,
            color: None,
            threads_unread: 0,
            threads_total: 0,
            messages_total: 0,
            hidden: false,
        }],
        ..Snapshot::default()
    };
    assert!(b.restore(snap.clone()));
    assert_eq!(
        (b.history_id, b.synced_once, b.resumed),
        (None, false, false)
    );
    let mut c = AccountState::new("rt".into(), None);
    c.restore(Snapshot {
        history_id: Some(5),
        ..snap
    });
    assert_eq!(
        (c.history_id, c.synced_once, c.resumed),
        (Some(5), true, true)
    );
}

/// With no connection a thread saved earlier still opens, even when the list has seen it
/// change since; one that was never saved fails as before, and a thread Gmail says is
/// gone is forgotten.
#[test]
fn saved_threads_open_offline() {
    let r = Relaunch::new("offline");
    r.run(Relaunch::first_script(), true);
    let (mut w, rx) = worker(r.secrets.clone(), nowhere());
    w.cache_dir = Some(r.cache_dir.clone());
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), None),
    );
    let key = ThreadKey::new("me@example.com", A);
    let known = |w: &mut Worker, history_id: u64| {
        w.accounts.get_mut("me@example.com").unwrap().known.insert(
            A.into(),
            Known {
                history_id,
                star_target: None,
                summary: parse::thread_summary(
                    &"me@example.com".to_string(),
                    &serde_json::from_str(&meta(A)).unwrap(),
                )
                .unwrap(),
            },
        );
    };
    // Changed since saved (the saved copy is at 123450), and nothing answers.
    known(&mut w, 999999);
    w.open_thread(key.clone(), false);
    let events = drain(&rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            SyncEvent::ThreadDetail(d) if d.key == key && d.messages.len() == 2
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SyncEvent::ThreadFailed { .. }))
    );
    // Never saved: the error shows.
    w.open_thread(ThreadKey::new("me@example.com", B), false);
    assert!(
        drain(&rx)
            .iter()
            .any(|e| matches!(e, SyncEvent::ThreadFailed { .. }))
    );
}

/// Nothing readable lands on disk: not the subject, the body, an address or the id.
#[test]
fn saved_threads_are_sealed_on_disk() {
    let r = Relaunch::new("sealed");
    r.run(Relaunch::first_script(), true);
    let mut files = Vec::new();
    let mut dirs = vec![r.cache_dir.clone()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else {
                files.push(p);
            }
        }
    }
    assert!(
        files
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "thread"))
    );
    for p in files {
        let name = p.to_string_lossy().into_owned();
        assert!(!name.contains("example") && !name.contains(A), "{name}");
        let raw = std::fs::read(&p).unwrap();
        for needle in ["Caf", "plans", "example.com", A] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} in {name}"
            );
        }
    }
    // Signing out removes the thread store with the snapshot, with no key needed.
    let (mut w, _rx) = worker(r.secrets.clone(), nowhere());
    w.cache_dir = Some(r.cache_dir.clone());
    w.accounts.insert(
        "me@example.com".into(),
        AccountState::new("rt".into(), None),
    );
    w.remove_account("me@example.com", Revoke::LocalOnly);
    assert_eq!(std::fs::read_dir(&r.cache_dir).unwrap().count(), 0);
}

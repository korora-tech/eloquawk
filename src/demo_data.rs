//! The generated demo mailbox (SPEC 11): two accounts, Gmail's system labels plus a few
//! coloured user labels, and threads covering every path the UI has to render (HTML
//! conversion, quote folding, long bodies, attachments, non-ASCII, Spam and Trash).
//!
//! Generation is seeded and deterministic; only the dates move, relative to the `now` passed
//! in. Nothing here touches the network or the disk.

use std::collections::BTreeSet;

use jiff::{SignedDuration, Timestamp};

use crate::mail::{address, html};
use crate::model::{
    AccountId, Address, Attachment, BodySource, Label, LabelKind, Message, ThreadDetail, ThreadKey,
    ThreadSummary, labels,
};

pub const PERSONAL: &str = "you@example.com";
pub const WORK: &str = "you@work.example";

/// Generic personal threads, on top of the special ones.
const PERSONAL_GENERIC: usize = 108;
/// Generic work threads, on top of the special ones.
const WORK_GENERIC: usize = 36;
/// The oldest generated mail.
const MAX_AGE_MINUTES: i64 = 400 * 24 * 60;

/// User label ids and names shared by both accounts: (id, name, (text, background)).
const USER_LABELS: &[(&str, &str, (u32, u32))] = &[
    ("Label_1", "Receipts", (0xffffff, 0x16a766)),
    ("Label_2", "Travel", (0xffffff, 0x3c78d8)),
    ("Label_3", "Projects", (0x000000, 0xfad165)),
    ("Label_4", "Projects/Eloquawk", (0xffffff, 0x8e63ce)),
    ("Label_5", "Projects/Website", (0xffffff, 0xe66550)),
];
pub const RECEIPTS: &str = "Label_1";
pub const TRAVEL: &str = "Label_2";
pub const PROJECT_ELOQUAWK: &str = "Label_4";
pub const PROJECT_WEBSITE: &str = "Label_5";

/// One thread: its summary and its full detail.
pub type DemoThread = (ThreadSummary, ThreadDetail);

/// Everything the demo worker serves.
pub struct Mailbox {
    pub accounts: Vec<AccountId>,
    pub threads: Vec<DemoThread>,
}

/// A small deterministic xorshift generator, so the mailbox is the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const CONTACTS: &[(&str, &str)] = &[
    ("Zoë Ångström", "zoe@angstrom.example"),
    ("Priya Raman", "priya@raman.example"),
    ("Marcus Webb", "marcus.webb@mail.example"),
    ("Søren Holm", "soren@holm.example"),
    ("Aiko Tanaka", "aiko.tanaka@post.example"),
    ("Tomás Ruiz", "tomas@ruiz.example"),
    ("Lena Fischer", "lena.fischer@mail.example"),
    ("Olu Adeyemi", "olu@adeyemi.example"),
    ("Fatima Zahra", "fatima.zahra@post.example"),
    ("Grace O'Neill", "grace@oneill.example"),
];

const COLLEAGUES: &[(&str, &str)] = &[
    ("Ada Brooks", "ada.brooks@work.example"),
    ("Kwame Mensah", "kwame.mensah@work.example"),
    ("Ines Duarte", "ines.duarte@work.example"),
    ("Rui Costa", "rui.costa@work.example"),
    ("Build bot", "ci@work.example"),
    ("Hannah Lee", "hannah.lee@partner.example"),
];

const PERSONAL_SUBJECTS: &[&str] = &[
    "Dinner on Friday?",
    "Photos from the weekend",
    "Book club: next pick",
    "Your order has shipped",
    "Re: Flat viewing on Saturday",
    "Climbing this week",
    "Birthday plans for Mum",
    "Recipe you asked for",
    "Running club schedule",
    "Quick question about the bike",
    "Holiday cottage options",
    "Concert tickets",
    "Garden allotment waiting list",
    "Moving boxes",
    "Your monthly statement is ready",
    "Podcast recommendation",
    "Lunch next week",
    "Spare key",
];

const WORK_SUBJECTS: &[&str] = &[
    "Quarterly planning notes",
    "Design review: settings screen",
    "Incident follow-up",
    "Interview loop for Thursday",
    "Release checklist",
    "Budget draft v3",
    "Team offsite agenda",
    "Customer feedback round-up",
    "On-call handover",
    "Security training reminder",
];

const LINES: &[&str] = &[
    "Hope you're well.",
    "Let me know what works for you.",
    "I've put the details below so they're all in one place.",
    "No rush on this, whenever you get a moment.",
    "Thanks again for sorting this out so quickly.",
    "I think the second option is the better one, but I'm happy either way.",
    "Shall we say half past six?",
    "I'll bring the rest on Saturday.",
    "Can you check the dates on your side?",
    "Here's the link I mentioned: https://example.org/notes",
    "The forecast looks good, so fingers crossed.",
    "Ping me if anything's unclear.",
];

fn addr(name: &str, email: &str) -> Address {
    Address::new(Some(name), email)
}

fn me(account: &str) -> Address {
    Address::new(Some("You"), account)
}

/// The account's labels with zero counts; [`with_counts`] fills them in.
pub fn label_defs(account: &str) -> Vec<Label> {
    let system = |id: &str, name: &str| Label {
        account: account.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        kind: LabelKind::System,
        color: None,
        threads_unread: 0,
        threads_total: 0,
        messages_total: 0,
        hidden: false,
    };
    let mut out = vec![
        system(labels::INBOX, "INBOX"),
        system(labels::STARRED, "STARRED"),
        system(labels::IMPORTANT, "IMPORTANT"),
        system(labels::SENT, "SENT"),
        system(labels::DRAFT, "DRAFT"),
        system(labels::SPAM, "SPAM"),
        system(labels::TRASH, "TRASH"),
        system(labels::UNREAD, "UNREAD"),
        system("CATEGORY_PERSONAL", "CATEGORY_PERSONAL"),
    ];
    out.extend(USER_LABELS.iter().map(|(id, name, color)| Label {
        account: account.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        kind: LabelKind::User,
        color: Some(*color),
        threads_unread: 0,
        threads_total: 0,
        messages_total: 0,
        hidden: false,
    }));
    out
}

/// `defs` with counts recomputed from the account's threads.
pub fn with_counts<'a>(
    defs: &[Label],
    threads: impl Iterator<Item = &'a ThreadSummary> + Clone,
) -> Vec<Label> {
    defs.iter()
        .map(|l| {
            let mut l = l.clone();
            let tagged = threads.clone().filter(|t| t.in_label(&l.id));
            l.threads_total = 0;
            l.threads_unread = 0;
            l.messages_total = 0;
            for t in tagged {
                l.threads_total += 1;
                l.threads_unread += u32::from(t.unread());
                l.messages_total += t.message_count;
            }
            l
        })
        .collect()
}

/// A thread's summary, derived from its messages the way `mail::parse` derives it from Gmail.
pub fn summary_of(detail: &ThreadDetail, history_id: u64) -> ThreadSummary {
    let label_ids: BTreeSet<String> = detail
        .messages
        .iter()
        .flat_map(|m| m.label_ids.iter().cloned())
        .collect();
    let last = detail.messages.last();
    ThreadSummary {
        key: detail.key.clone(),
        history_id,
        subject: detail.subject.clone(),
        snippet: last.map(|m| m.snippet.clone()).unwrap_or_default(),
        participants: address::participants(
            detail
                .messages
                .iter()
                .map(|m| (m.from.clone(), m.label_ids.contains(labels::UNREAD))),
        ),
        last_date: detail
            .messages
            .iter()
            .map(|m| m.date)
            .max()
            .unwrap_or(Timestamp::UNIX_EPOCH),
        message_count: detail.messages.len() as u32,
        label_ids,
        has_attachments: detail.messages.iter().any(|m| !m.attachments.is_empty()),
    }
}

/// The first 120 characters of a body with whitespace collapsed, as Gmail's snippet.
pub fn snippet_of(body: &str) -> String {
    let flat: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .flat_map(|l| l.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    flat.chars().take(120).collect()
}

/// The generated bytes of a demo attachment: `size` bytes derived from its name, so forwarding
/// one sends the same bytes every time.
pub fn attachment_bytes(a: &Attachment) -> Vec<u8> {
    let seed = a
        .filename
        .bytes()
        .fold(0x9e37_79b9_u32, |h, b| h.rotate_left(5) ^ u32::from(b));
    (0..a.size)
        .map(|i| (seed.wrapping_add(i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect()
}

/// One message of a thread under construction.
pub struct Draft {
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub body: String,
    pub source: BodySource,
    pub truncated: bool,
    pub minutes_ago: i64,
    pub unread: bool,
    pub attachments: Vec<Attachment>,
}

impl Draft {
    fn new(from: Address, to: Address, body: impl Into<String>, minutes_ago: i64) -> Self {
        Self {
            from,
            to: vec![to],
            cc: vec![],
            body: body.into(),
            source: BodySource::Plain,
            truncated: false,
            minutes_ago,
            unread: false,
            attachments: vec![],
        }
    }

    fn unread(mut self) -> Self {
        self.unread = true;
        self
    }

    fn attach(mut self, filename: &str, mime: &str, size: u64) -> Self {
        let n = self.attachments.len() + 1;
        self.attachments.push(Attachment {
            part_id: n.to_string(),
            attachment_id: Some(format!("demo-att-{n}")),
            filename: filename.into(),
            mime_type: mime.into(),
            size,
        });
        self
    }
}

/// Builds a thread. `thread_labels` go on every message from someone else (STARRED only on
/// the last one); messages from the account itself carry SENT instead of INBOX.
pub fn build_thread(
    account: &str,
    id: &str,
    subject: &str,
    thread_labels: &[&str],
    drafts: Vec<Draft>,
    now: Timestamp,
) -> ThreadDetail {
    let n = drafts.len();
    let mut references: Vec<String> = Vec::new();
    let mut messages = Vec::with_capacity(n);
    for (i, d) in drafts.into_iter().enumerate() {
        let from_me = d.from.email.eq_ignore_ascii_case(account);
        let mut label_ids: BTreeSet<String> = thread_labels
            .iter()
            .filter(|l| **l != labels::STARRED || i + 1 == n)
            .filter(|l| !(from_me && (**l == labels::INBOX)))
            .map(|l| l.to_string())
            .collect();
        if from_me {
            label_ids.insert(labels::SENT.into());
        } else if d.unread {
            label_ids.insert(labels::UNREAD.into());
        }
        let message_id = format!("<{id}.{i}@demo.eloquawk>");
        let subject = if i == 0 || subject.starts_with("Re:") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        };
        messages.push(Message {
            id: format!("{id}-m{i}"),
            thread_id: id.to_string(),
            label_ids,
            from: d.from,
            to: d.to,
            cc: d.cc,
            bcc: vec![],
            reply_to: vec![],
            date: now - SignedDuration::from_mins(d.minutes_ago),
            subject,
            message_id: Some(message_id.clone()),
            in_reply_to: references.last().cloned(),
            references: references.clone(),
            snippet: snippet_of(&d.body),
            body: d.body,
            body_source: d.source,
            body_truncated: d.truncated,
            signals: Default::default(),
            attachments: d.attachments,
        });
        references.push(message_id);
    }
    let key = ThreadKey::new(account, id);
    let label_ids = messages
        .iter()
        .flat_map(|m| m.label_ids.iter().cloned())
        .collect();
    ThreadDetail {
        key,
        subject: subject.to_string(),
        label_ids,
        messages,
    }
}

/// Quotes `body` the way a mail client does in a reply.
fn quote(name: &str, body: &str) -> String {
    let mut out = format!("On an earlier day, {name} wrote:\n");
    for line in body.lines() {
        if line.is_empty() {
            out.push_str(">\n");
        } else if line.starts_with('>') {
            out.push('>');
            out.push_str(line);
            out.push('\n');
        } else {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Minutes ago for generic thread `i` of `n`: dense near now, thinning out to 400 days.
fn age_minutes(i: usize, n: usize) -> i64 {
    let f = (i as f64 + 1.0) / n as f64;
    (45.0 + f * f * MAX_AGE_MINUTES as f64) as i64
}

fn paragraph(rng: &mut Rng, greeting: &str) -> String {
    let mut body = format!("Hi {greeting},\n\n");
    let count = 2 + rng.below(3);
    for _ in 0..count {
        body.push_str(rng.pick(LINES));
        body.push(' ');
    }
    body.pop();
    body.push_str("\n\n");
    body
}

const NEWSLETTER_HTML: &str = r#"<html><head><style>p { color: red }</style>
<script>trackOpen()</script></head><body>
<h1>The Weekly Byte &mdash; issue 42</h1>
<p>Good morning! This week: faster builds, quieter notifications and a tour of the new
GPU text renderer.</p>
<img src="https://tracker.example/open.gif" width="1" height="1">
<h2>Faster builds</h2>
<p>Incremental compilation got another 12% quicker. <a href="https://example.org/builds">Read
the full write-up</a> for the numbers.</p>
<h2>Quieter notifications</h2>
<p>Notifications now group when more than three arrive at once. Details are on
<a href="https://example.org/notifications">https://example.org/notifications</a>.</p>
<h2>Account security</h2>
<p>We will never ask for your password. If an email says otherwise, check where its links
really go, like this one: <a href="https://phish.example/login">https://bank.example/login</a>.</p>
<ul><li>Builds: 12% faster</li><li>Notifications: grouped</li><li>Text: crisper</li></ul>
<p>Thanks for reading &hellip; see you next week.<br>
<a href="https://example.org/unsubscribe">Unsubscribe</a></p>
</body></html>"#;

fn newsletter(you: &Address, now_minutes: i64) -> Draft {
    let (body, truncated) = match html::html_to_text(NEWSLETTER_HTML) {
        Some(c) => (c.text, c.truncated),
        None => ("The Weekly Byte, issue 42".to_string(), true),
    };
    let mut d = Draft::new(
        addr("The Weekly Byte", "news@weeklybyte.example"),
        you.clone(),
        body,
        now_minutes,
    )
    .unread();
    d.source = BodySource::Html;
    d.truncated = truncated;
    d
}

fn long_log_body() -> String {
    let mut body = String::from("Here are last night's logs, as promised.\n\n");
    let mut i = 0u32;
    while body.len() < 210_000 {
        body.push_str(&format!(
            "2026-10-28T23:{:02}:{:02}Z web-{} GET /api/v1/threads?page={} 200 {}ms\n",
            (i / 60) % 60,
            i % 60,
            i % 4,
            i % 37,
            5 + i % 90
        ));
        i += 1;
    }
    body
}

fn deep_quote_chain(account: &str, you: &Address, now: Timestamp) -> ThreadDetail {
    let soren = addr("Søren Holm", "soren@holm.example");
    let lines = [
        "Could we look at venues for the offsite? I'd like somewhere near a station.",
        "The boathouse is free on the 14th. It holds forty people.",
        "Forty is tight. Is the barn still an option?",
        "The barn is booked, but the boathouse has a second room.",
        "Perfect, let's hold both rooms.",
        "Held until Friday. I'll confirm numbers on Thursday.",
    ];
    let mut drafts = Vec::new();
    let mut previous = String::new();
    for (i, line) in lines.iter().enumerate() {
        let (from, to) = if i % 2 == 0 {
            (soren.clone(), you.clone())
        } else {
            (you.clone(), soren.clone())
        };
        let body = if previous.is_empty() {
            format!("{line}\n")
        } else {
            let prev_name = if i % 2 == 0 { "You" } else { "Søren Holm" };
            format!("{line}\n\n{}", quote(prev_name, &previous))
        };
        drafts.push(Draft::new(
            from,
            to,
            body.clone(),
            3 * 24 * 60 - (i as i64) * 90,
        ));
        previous = body;
    }
    build_thread(
        account,
        "p-offsite",
        "Venue for the offsite",
        &[labels::INBOX, PROJECT_WEBSITE],
        drafts,
        now,
    )
}

/// The special personal threads, newest first.
fn personal_specials(now: Timestamp) -> Vec<ThreadDetail> {
    let a = PERSONAL;
    let you = me(a);
    let zoe = addr("Zoë Ångström", "zoe@angstrom.example");
    let priya = addr("Priya Raman", "priya@raman.example");
    let mut out = vec![
        build_thread(
            a,
            "p-cafe",
            "Café ☕ plans",
            &[labels::INBOX, labels::IMPORTANT],
            vec![
                Draft::new(
                    you.clone(),
                    zoe.clone(),
                    "Fancy a coffee this week? The new place on Elm Street does a good flat white.\n",
                    95,
                ),
                Draft::new(
                    zoe.clone(),
                    you.clone(),
                    format!(
                        "Yes please! Thursday at ten?\n\nZoë\n\n{}",
                        quote(
                            "You",
                            "Fancy a coffee this week? The new place on Elm Street does a good flat white."
                        )
                    ),
                    4,
                )
                .unread(),
            ],
            now,
        ),
        build_thread(
            a,
            "p-fail",
            "[fail] This conversation refuses every change",
            &[labels::INBOX],
            vec![
                Draft::new(
                    addr("Demo", "demo@eloquawk.example"),
                    you.clone(),
                    "Any change to this conversation fails in demo mode, so you can watch the \
                     optimistic update roll back.\n",
                    25,
                )
                .unread(),
            ],
            now,
        ),
        build_thread(
            a,
            "p-code",
            "Your sign-in code and parcel",
            &[labels::INBOX],
            vec![
                Draft::new(
                    addr("Parcel & Co", "noreply@parcel.example"),
                    you.clone(),
                    "Your verification code is 482913. It expires in 10 minutes.\n\n\
                     Your parcel is on its way. Tracking number: 1Z999AA10123456784\n\n\
                     Track it at https://parcel.example/track/1Z999AA10123456784\n",
                    40,
                )
                .unread(),
            ],
            now,
        ),
        build_thread(
            a,
            "p-news",
            "The Weekly Byte — issue 42",
            &[labels::INBOX],
            vec![newsletter(&you, 180)],
            now,
        ),
        build_thread(
            a,
            "p-logs",
            "Server logs from last night",
            &[labels::INBOX, PROJECT_ELOQUAWK],
            vec![
                Draft::new(
                    addr("Marcus Webb", "marcus.webb@mail.example"),
                    you.clone(),
                    long_log_body(),
                    7 * 60,
                )
                .attach("web-1.log", "text/plain", 48_213),
            ],
            now,
        ),
        build_thread(
            a,
            "p-flight",
            "Flight confirmation: LHR → OSL",
            &[labels::INBOX, labels::STARRED, TRAVEL],
            vec![
                Draft::new(
                    addr("Nordic Air", "bookings@nordicair.example"),
                    you.clone(),
                    "Booking reference: QX7P2L\nDeparts 08:40 from London Heathrow, Terminal 2.\n\
                     Arrives 11:45 at Oslo Gardermoen.\n",
                    26 * 60,
                )
                .attach("boarding-pass.pdf", "application/pdf", 182_400),
            ],
            now,
        ),
        deep_quote_chain(a, &you, now),
        build_thread(
            a,
            "p-receipt",
            "Your receipt from Corner Books",
            &[RECEIPTS],
            vec![
                Draft::new(
                    addr("Corner Books", "receipts@cornerbooks.example"),
                    you.clone(),
                    "Thank you for your order.\n\n1 × The Left Hand of Darkness   £9.99\n\
                     Total paid: £9.99\n",
                    5 * 24 * 60,
                )
                .attach("receipt-10442.pdf", "application/pdf", 64_120),
            ],
            now,
        ),
        build_thread(
            a,
            "p-sent",
            "Spare key for the flat",
            &[],
            vec![Draft::new(
                you.clone(),
                priya.clone(),
                "I've left the spare key with the neighbour at number 12.\n",
                2 * 24 * 60,
            )],
            now,
        ),
        build_thread(
            a,
            "p-draft",
            "Notes for the landlord",
            &[labels::DRAFT],
            vec![Draft::new(
                you.clone(),
                addr("Landlord", "lettings@flats.example"),
                "The boiler pressure keeps dropping, and the bathroom fan…\n",
                9 * 60,
            )],
            now,
        ),
        build_thread(
            a,
            "p-spam1",
            "You have WON a luxury cruise",
            &[labels::SPAM],
            vec![
                Draft::new(
                    addr("Prize Desk", "winner@prizes.example"),
                    you.clone(),
                    "Claim your prize now at https://prizes.example/claim before it expires!\n",
                    3 * 60,
                )
                .unread(),
            ],
            now,
        ),
        build_thread(
            a,
            "p-spam2",
            "Urgent: verify your account",
            &[labels::SPAM],
            vec![Draft::new(
                addr("Security Team", "security@verify-now.example"),
                you.clone(),
                "Your mailbox will be closed unless you verify it today.\n",
                2 * 24 * 60,
            )],
            now,
        ),
        build_thread(
            a,
            "p-trash1",
            "Old newsletter you deleted",
            &[labels::TRASH],
            vec![Draft::new(
                addr("Gadget Weekly", "hello@gadgets.example"),
                you.clone(),
                "This week's gadgets are the best gadgets yet.\n",
                6 * 24 * 60,
            )],
            now,
        ),
        build_thread(
            a,
            "p-trash2",
            "Re: Car share",
            &[labels::TRASH],
            vec![Draft::new(
                addr("Tomás Ruiz", "tomas@ruiz.example"),
                you.clone(),
                "Never mind, I found a lift.\n",
                12 * 24 * 60,
            )],
            now,
        ),
    ];
    out.push(build_thread(
        a,
        "p-photos",
        "Photos from Lofoten",
        &[labels::INBOX, TRAVEL],
        vec![
            Draft::new(
                zoe.clone(),
                you.clone(),
                "A few favourites from the trip. The big one is the view from Reinebringen.\n",
                30 * 24 * 60,
            )
            .attach("reine.jpg", "image/jpeg", 2_480_133)
            .attach("hamnoy.jpg", "image/jpeg", 1_934_201),
        ],
        now,
    ));
    out
}

fn work_specials(now: Timestamp) -> Vec<ThreadDetail> {
    let a = WORK;
    let you = me(a);
    let ada = addr("Ada Brooks", "ada.brooks@work.example");
    let kwame = addr("Kwame Mensah", "kwame.mensah@work.example");
    vec![
        build_thread(
            a,
            "w-roadmap",
            "Roadmap review tomorrow",
            &[labels::INBOX, labels::IMPORTANT, PROJECT_ELOQUAWK],
            vec![
                Draft::new(
                    ada.clone(),
                    you.clone(),
                    "Slides attached. Please add your section by tonight.\n",
                    70,
                )
                .unread()
                .attach("roadmap-q4.pdf", "application/pdf", 1_204_551),
            ],
            now,
        ),
        build_thread(
            a,
            "w-incident",
            "Incident 2291: elevated error rates",
            &[labels::INBOX, labels::STARRED],
            vec![
                Draft::new(
                    kwame.clone(),
                    you.clone(),
                    "Error rates are back to normal. Write-up to follow.\n",
                    5 * 60,
                ),
                Draft::new(
                    you.clone(),
                    kwame.clone(),
                    "Thanks Kwame. I'll review the timeline in the morning.\n",
                    4 * 60,
                ),
                Draft::new(
                    kwame.clone(),
                    you.clone(),
                    "Draft timeline is in the doc. Two follow-ups for us.\n",
                    3 * 60,
                )
                .unread(),
            ],
            now,
        ),
        build_thread(
            a,
            "w-expenses",
            "Expenses approved",
            &[RECEIPTS],
            vec![Draft::new(
                addr("Finance", "finance@work.example"),
                you.clone(),
                "Your October expenses have been approved and will be paid on the 30th.\n",
                9 * 24 * 60,
            )],
            now,
        ),
        build_thread(
            a,
            "w-spam",
            "Increase your website traffic",
            &[labels::SPAM],
            vec![Draft::new(
                addr("SEO Experts", "growth@seo.example"),
                you.clone(),
                "We can put you on the first page of every search engine.\n",
                24 * 60,
            )],
            now,
        ),
    ]
}

/// Generic threads of one to six messages with the account's correspondents.
fn generic(
    account: &str,
    rng: &mut Rng,
    count: usize,
    people: &[(&str, &str)],
    subjects: &[&str],
    prefix: &str,
    now: Timestamp,
) -> Vec<ThreadDetail> {
    let you = me(account);
    (0..count)
        .map(|i| {
            let (name, email) = *rng.pick(people);
            let them = addr(name, email);
            let first = name.split_whitespace().next().unwrap_or(name);
            let subject = *rng.pick(subjects);
            let age = age_minutes(i, count) + 60 * (i as i64 % 5);
            let max_messages = if rng.chance(30) { 6 } else { 2 };
            let messages = 1 + rng.below(max_messages);
            let recent = i < count / 3;
            let mut drafts: Vec<Draft> = Vec::new();
            let mut previous: Option<(String, String)> = None;
            for m in 0..messages {
                // Alternate: them, you, them... ending with them most of the time.
                let from_them = (messages - 1 - m).is_multiple_of(2);
                let (from, to, greeting) = if from_them {
                    (them.clone(), you.clone(), "there")
                } else {
                    (you.clone(), them.clone(), first)
                };
                let mut body = paragraph(rng, greeting);
                body.push_str(if from_them { first } else { "Me" });
                body.push('\n');
                if let Some((prev_name, prev_body)) = &previous {
                    body.push('\n');
                    body.push_str(&quote(prev_name, prev_body));
                }
                let minutes_ago = age + ((messages - 1 - m) as i64) * 47;
                let mut d = Draft::new(from, to, body.clone(), minutes_ago);
                if from_them && m + 1 == messages && recent && rng.chance(35) {
                    d = d.unread();
                }
                if from_them && rng.chance(8) {
                    d = d.attach(
                        "notes.pdf",
                        "application/pdf",
                        20_000 + rng.below(400_000) as u64,
                    );
                }
                previous = Some((
                    if from_them {
                        name.to_string()
                    } else {
                        "You".to_string()
                    },
                    body,
                ));
                drafts.push(d);
            }
            let mut thread_labels: Vec<&str> = Vec::new();
            if rng.chance(78) {
                thread_labels.push(labels::INBOX);
            }
            if rng.chance(10) {
                thread_labels.push(labels::STARRED);
            }
            if rng.chance(12) {
                thread_labels.push(labels::IMPORTANT);
            }
            match rng.below(12) {
                0 => thread_labels.push(RECEIPTS),
                1 => thread_labels.push(TRAVEL),
                2 => thread_labels.push(PROJECT_ELOQUAWK),
                3 => thread_labels.push(PROJECT_WEBSITE),
                _ => {}
            }
            build_thread(
                account,
                &format!("{prefix}{i:03}"),
                subject,
                &thread_labels,
                drafts,
                now,
            )
        })
        .collect()
}

/// The whole demo mailbox, with dates relative to `now`.
pub fn generate(now: Timestamp) -> Mailbox {
    let mut rng = Rng(0x0e10_9a3a_7c11_2026);
    let mut details = personal_specials(now);
    details.extend(generic(
        PERSONAL,
        &mut rng,
        PERSONAL_GENERIC,
        CONTACTS,
        PERSONAL_SUBJECTS,
        "p-",
        now,
    ));
    details.extend(work_specials(now));
    details.extend(generic(
        WORK,
        &mut rng,
        WORK_GENERIC,
        COLLEAGUES,
        WORK_SUBJECTS,
        "w-",
        now,
    ));
    let threads = details
        .into_iter()
        .enumerate()
        .map(|(i, d)| (summary_of(&d, 1000 + i as u64), d))
        .collect();
    Mailbox {
        accounts: vec![PERSONAL.into(), WORK.into()],
        threads,
    }
}

/// The unread INBOX thread the demo delivers 20 s after start (SPEC 11).
pub fn new_mail_thread(account: &str, now: Timestamp) -> ThreadDetail {
    build_thread(
        account,
        "p-new-mail",
        "Are you free for a quick call?",
        &[labels::INBOX],
        vec![
            Draft::new(
                addr("Priya Raman", "priya@raman.example"),
                me(account),
                "Something came up about the flat. Have you got ten minutes this afternoon?\n",
                0,
            )
            .unread(),
        ],
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-10-29T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn mailbox_is_deterministic_and_sized() {
        let a = generate(now());
        let b = generate(now());
        assert_eq!(a.threads.len(), b.threads.len());
        assert!(a.threads.iter().zip(&b.threads).all(|(x, y)| x == y));
        let personal = a.threads.iter().filter(|t| t.0.key.account == PERSONAL);
        let work = a.threads.iter().filter(|t| t.0.key.account == WORK);
        let (p, w) = (personal.count(), work.count());
        assert!((110..=130).contains(&p), "personal {p}");
        assert!((35..=45).contains(&w), "work {w}");
        let inbox = a
            .threads
            .iter()
            .filter(|t| t.0.key.account == PERSONAL && t.0.in_label(labels::INBOX))
            .count();
        assert!(inbox > 60, "inbox {inbox}");
    }

    #[test]
    fn ids_are_unique_per_account() {
        let m = generate(now());
        let keys: BTreeSet<_> = m.threads.iter().map(|t| t.0.key.clone()).collect();
        assert_eq!(keys.len(), m.threads.len());
    }

    #[test]
    fn special_content_is_present() {
        let m = generate(now());
        let find = |id: &str| {
            m.threads
                .iter()
                .find(|t| t.0.key.id == id)
                .unwrap_or_else(|| panic!("missing {id}"))
        };
        let news = &find("p-news").1.messages[0];
        assert_eq!(news.body_source, BodySource::Html);
        assert!(
            news.body
                .contains("https://bank.example/login <https://phish.example/login>")
        );
        assert!(!news.body.contains("trackOpen"));
        assert!(find("p-logs").1.messages[0].body.len() > 200_000);
        assert!(find("p-code").1.messages[0].body.contains("482913"));
        assert!(
            find("p-offsite")
                .1
                .messages
                .last()
                .unwrap()
                .body
                .contains(">>")
        );
        assert!(find("p-cafe").0.unread());
        assert!(find("p-spam1").0.in_label(labels::SPAM));
        assert!(find("p-trash1").0.in_label(labels::TRASH));
        assert!(find("p-photos").0.has_attachments);
        let ages: Vec<_> = m
            .threads
            .iter()
            .map(|t| now().duration_since(t.0.last_date))
            .collect();
        assert!(ages.iter().any(|d| d.as_mins() < 10));
        assert!(ages.iter().any(|d| d.as_hours() > 24 * 300));
    }

    #[test]
    fn newest_inbox_thread_is_unread() {
        let m = generate(now());
        let newest = m
            .threads
            .iter()
            .filter(|t| t.0.key.account == PERSONAL && t.0.in_label(labels::INBOX))
            .max_by_key(|t| t.0.last_date)
            .unwrap();
        assert!(newest.0.unread());
    }

    #[test]
    fn sent_messages_skip_inbox_and_unread() {
        let m = generate(now());
        let sent = &m.threads.iter().find(|t| t.0.key.id == "p-sent").unwrap().0;
        assert!(sent.in_label(labels::SENT));
        assert!(!sent.in_label(labels::INBOX) && !sent.unread());
    }

    #[test]
    fn counts_follow_threads() {
        let m = generate(now());
        let summaries: Vec<&ThreadSummary> = m
            .threads
            .iter()
            .filter(|t| t.0.key.account == PERSONAL)
            .map(|t| &t.0)
            .collect();
        let labels = with_counts(&label_defs(PERSONAL), summaries.iter().copied());
        let inbox = labels.iter().find(|l| l.id == "INBOX").unwrap();
        assert!(inbox.threads_unread > 0);
        assert_eq!(
            inbox.threads_total as usize,
            summaries.iter().filter(|s| s.in_label("INBOX")).count()
        );
    }
}

# Eloquawk: implementation specification

Eloquawk is a native Gmail client for Linux, written in Rust with GPUI. This document is the
contract for the agents building it. Each milestone in section 13 is built, checked and
committed on its own. Where this spec and a sibling app disagree, this spec wins. Where this
spec says nothing, do what `korora-tech/antarctical` does.

Reference app: `/home/rawkode/Code/src/github.com/korora-tech/antarctical` (called
**antarctical** below). "Copy+adapt" means copy that file, rename identifiers and strings, and
delete what is calendar-specific. Do not import code from rockhopper or flipper. They are
built on gpui-kit, whose API differs from the pinned zed gpui.

---

## 1. Ground rules

- **Stack:** single crate (bin only, no `lib.rs`), edition 2024, `rust-toolchain.toml` channel
  `stable` (the instance lock needs 1.89+ for `File::try_lock`). gpui and gpui_platform are
  pinned to the zed git rev `28d0a3547f16b9609b301b48ab363bfca649c864` with
  `default-features = false`, and gpui_platform has `features = ["wayland", "x11"]`. The entry
  point is `gpui_platform::application()`.
- **Never in the dependency tree:** tokio. `cargo tree -i tokio` must print nothing at every
  milestone. The check applies to the host (Linux) target: `--target all` shows tokio via
  wasm-bindgen-futures from gpui_web and wgpu, which is wasm-only and also in antarctical's
  lock. HTTP is `ureq = "=3.5.0"` (rustls + json) on the sync worker thread.
- **App id:** `korora.tech.Eloquawk` is used everywhere: Wayland `app_id`, the desktop file
  name, `StartupWMClass`, the hicolor icon name, the notification icon and the keyring service.
- **Secrets:** refresh tokens and a user-supplied OAuth client live only in the Secret
  Service. Access tokens live only in worker memory. Mail content is never written to disk or
  to logs, except an attachment the user explicitly saves. Prefs hold no mail data apart from
  the last selected label id.
- **Built-in OAuth client (M9 change).** Eloquawk compiles in its own Desktop OAuth client
  (`oauth::builtin_client()`), exactly as antarctical does: a desktop client's secret ships in
  every binary and is public by nature (Google says so; PKCE protects the sign-in), so it is
  committed on purpose. Compile-time `option_env!("ELOQUAWK_GOOGLE_CLIENT_ID")` /
  `option_env!("ELOQUAWK_GOOGLE_CLIENT_SECRET")` override each part, and a client pasted in
  Settings and saved to the keyring overrides both. The "Set up a Google client" card appears
  only when the built-in client ends up empty (an override set to `""`, or an empty compiled
  default) and none is saved. Never reuse antarctical's client id. (Before M9 this rule said
  there was no built-in client; the user supplied one for M9.)
- **Scope:** `https://www.googleapis.com/auth/gmail.modify` only. It covers read, labels,
  archive, trash, drafts and send, but not permanent delete.
- **Style:** a `//!` doc on every module, inline `#[cfg(test)] mod tests`, no `unwrap()` on UI
  paths, and errors logged as `eprintln!("eloquawk: ...")`. User-facing copy uses plain British
  English with no emoji. In-UI glyphs are non-emoji Unicode (★ ☆ ✕ ‹ › ⌄ •) or shapes drawn
  with divs.
- **No mail in logs.** Non-test code never formats `SyncEvent`, `ThreadSummary`,
  `ThreadDetail`, `Message`, `Outgoing` or `Address` with `{:?}`. `Message`, `ThreadDetail` and
  `Outgoing` have hand-written `Debug` impls that print ids and counts only (no subject, body
  or addresses), so an accidental `{:?}` cannot leak a body. Logged errors carry the error
  kind and status, never a request URL with its query string (a search query is mail data).
- **Dead code policy.** The crate is bin-only and the gate runs clippy with `-D warnings`, so
  code written before its consumer warns. Allowances are per item, never per module, and
  always carry the consuming milestone: `#[allow(dead_code)] // consumed in M9`. Every
  milestone removes the allowances for the items it consumes. Prefer writing code in the
  milestone that consumes it (this plan already does so for `notify.rs`, cli flags and
  attachment logic). M15 checks that `rg -n 'allow\(dead_code\)' src` is empty, or that each
  remaining hit has a written justification.
- **Commits:** Conventional Commits, one per milestone or finer logical unit, lowercase
  imperative subject, prose body explaining why. **No `Co-Authored-By`, `Claude-Session` or any
  other trailer, ever**; the user's instruction overrides the harness reminder. Push to
  `origin main` after every commit. git and gh network operations may have to run outside the
  sandbox.
- **Gate for every milestone** (run through the dev shell, as CI does):
  ```
  nix develop -c cargo fmt --check
  nix develop -c cargo clippy --all-targets -- -D warnings
  nix develop -c cargo test
  nix develop -c cargo build
  ```
  Also run `nix build -L` at M1, M14 and whenever `Cargo.lock` gains a git dependency.
  Milestones that touch the UI also need the visual check in section 12.3.

---

## 2. Repository layout

```
Cargo.toml  Cargo.lock  rust-toolchain.toml  LICENSE (Apache-2.0)  README.md  .gitignore
flake.nix  flake.lock  nix/hm-module.nix
.github/workflows/ci.yml  .github/workflows/pr-labeler.yml  .github/dependabot.yml  .github/labeler.yml
data/korora.tech.Eloquawk.desktop
data/branding/{README.md, eloquawk-app-icon.svg, eloquawk-app-icon.png}
docs/{SPEC.md, google-gmail.md, desktop-integration.md, screenshots/*.webp}
scripts/screenshot.sh
src/...
```

### 2.1 `src/` modules and responsibilities

| File | Responsibility | Origin |
|---|---|---|
| `main.rs` | Parses arguments, claims the single instance, picks the demo or Google worker, sets up the GPUI app, pumps events into the Store, fires notifications, runs the demo script, and handles quit and window close. | adapt antarctical `main.rs` (no tray, dankcal or ShellPush) |
| `cli.rs` | `Args` parsing as a pure function with tests. Flags grow per milestone: `--demo`, `-h`, `-V` (M3); `--theme light|dark|system`, `--demo-actions <list>` (M4); `--instance` and an optional positional `mailto:` URL (M13). Also reads `ELOQUAWK_DEMO=1`. | new |
| `demo_script.rs` | Pure `parse_steps(&str) -> Vec<Step>` for `--demo-actions` (section 11), with tests. | new |
| `branding.rs` | `APP_ID` (M1); the embedded SVG icon (`LazyLock<Arc<Image>>`) and `icon(size)` (M14). The PNG is installed but not embedded: with no tray nothing in the app decodes it (M14 change). | copy+adapt |
| `config.rs` | `Prefs` (section 4.3), XDG path `$XDG_CONFIG_HOME/eloquawk/settings.json`, atomic save, `ephemeral`. `load_from(path)` / `save_to(path)` take the path so tests use a temp dir. | copy+adapt |
| `secrets.rs` | `SecretStore` trait, `KeyringStore`, `MemoryStore`, and the typed `Secrets` API (client, token map). Debug builds honour `ELOQUAWK_SECRETS=memory`. | copy, with the legacy migration removed |
| `instance.rs` | Single instance: flock on `$XDG_RUNTIME_DIR/eloquawk.lock`, 0600 socket `eloquawk.sock`, JSON-line `hello` / `show` / `compose {mailto}`. `--demo --instance` uses `eloquawk-demo.lock` / `eloquawk-demo.sock`. | extracted from antarctical `dankcal.rs` |
| `notify.rs` | Pure `new_mail_notes(threads, &mut seen) -> Vec<Note>`, grouping, the body format and markup escaping. `show(note)` calls notify-rust and is only called from `background_spawn`. | new, modelled on `reminders.rs` |
| `model.rs` | Domain types (section 4.1). | new |
| `protocol.rs` | `Command`, `SyncEvent`, `AuthState`, `SyncHandle`, `OpId`, `ViewKey`, `PageMode`. The demo and Google workers share it. | split out of antarctical `sync.rs` |
| `store.rs` | The `Store` (plain struct wrapped in a GPUI entity), `apply(SyncEvent)`, queries, selection, page merging, and optimistic mutations with rollback and undo. | adapt antarctical `store.rs` |
| `demo.rs` | Demo worker thread `eloquawk-demo`. Answers every `Command` with an exhaustive `match`. | copy structure |
| `demo_data.rs` | Deterministic generated mailbox for two accounts. | new |
| `sync.rs` | Google worker thread `eloquawk-sync`: the job queue, sign-in, tokens, the sync algorithm, mutations, send and attachments. `Endpoints { base, upload, token }` is injectable for the fake-server tests. | adapt antarctical `sync.rs` |
| `google/mod.rs` | Re-exports. | — |
| `google/oauth.rs` | PKCE loopback sign-in, exchange, refresh, revoke, and the `ELOQUAWK_*` client. | copy+adapt |
| `google/api.rs` | ureq agent, `get` / `post_json` / `post_empty` / `post_multipart` helpers, `ApiError`, error-envelope parsing, retry hints. | adapt |
| `google/gmail.rs` | Gmail DTOs (serde) and endpoint functions (section 6.2). Every function takes the base URL. The conversion from DTOs to `model` types lives in `mail/parse.rs`. | new |
| `mail/mod.rs` | Re-exports. | — |
| `mail/b64.rs` | URL-safe base64 engine that accepts both padded and unpadded input (`DecodePaddingMode::Indifferent`), plus the encoder. | new |
| `mail/parse.rs` | Gmail `Message`/`Part` tree to `model::Message`: headers (case-insensitive), RFC 2047 fallback, body choice, charset, attachments. `ThreadDto` to `ThreadSummary`. | new |
| `mail/html.rs` | `html_to_text(html) -> Option<Converted { text, truncated }>` via html2text with an inline-links decorator, input cap, panic guard, tracking pixels and remote images dropped; `None` means the conversion failed and the caller uses the snippet (M2 note: the spec's `-> String` could not carry the truncation and failure flags). `decode_entities` for snippets. | new |
| `mail/links.rs` | Pure `find_links(text) -> Vec<(Range<usize>, LinkTarget)>`: URL and address detection, scheme allowlist (http, https, mailto). | new |
| `mail/address.rs` | Parses and formats `Name <a@b>` and address lists, handling quoted commas. `display_name`, `participants(..)`. | new |
| `mail/query.rs` | A subset of Gmail query syntax, parsed and matched locally (demo mode, plus optimistic list membership). | new |
| `mail/reply.rs` | Pure builders for the `Outgoing` prefill of reply, reply-all and forward: recipients, subject, quoting, threading headers. | new |
| `mail/compose.rs` | `Outgoing` to RFC 5322 bytes (lettre builder). | new |
| `mail/mailto.rs` | Parses a `mailto:` URL into an `Outgoing` prefill, with an hfield whitelist. | new |
| `mail/files.rs` | Pure attachment filename sanitising, unique naming and download-dir resolution. | new |
| `ui/mod.rs` | `actions!(eloquawk, [...])` and `bind_keys` (section 9). | adapt |
| `ui/theme.rs` | `Palette` struct, `DARK` / `LIGHT`, the `Theme` Global, `palette(cx)`, `tint`, `tabular`, contrast tests. | adapt |
| `ui/widgets.rs` | Buttons, ghost button, segments, checkbox row, card, caption, `label_row`, `badge`, `OnClick`. | copy+adapt |
| `ui/text_input.rs` | Single-line IME text input. Selection colours come from the palette. | copy+adapt |
| `ui/text_layout.rs` | Shared wrapped-text core: shaping with `shape_text` at a wrap width, measured layout, hit-testing (position ↔ byte offset), wrap boundaries as byte offsets, selection and link-underline painting. Used by `body_text.rs` and `text_area.rs`. | new |
| `ui/body_text.rs` | `BodyText` entity: read-only, selectable, copyable message body with clickable links (section 8.2). | new |
| `ui/text_area.rs` | Multi-line wrapped editor for compose bodies (section 8.6). | new |
| `ui/format.rs` | `list_date`, `full_date`, `size_human`, `time`. | adapt |
| `ui/windows.rs` | `show_main_window`, `open_compose_window(prefill)`. | adapt |
| `ui/main_view.rs` | Root view: header (search), banner, three panes, toast, screen switching, action handlers. | adapt skeleton |
| `ui/sidebar.rs` | Accounts with their labels, unread badges, compose button, account footer. | new |
| `ui/thread_list.rs` | Thread rows in a `uniform_list` with selection; the "load more" footer is a sibling below the list. | new |
| `ui/reader.rs` | `ReaderView` entity, embedded with `.cached(..)`: collapsible messages, quoted-text folding, attachments, action bar, its own scroll handle. | new |
| `ui/compose.rs` | `ComposeView` window: From, To, Cc, Bcc, Subject, body `TextArea`, attachments, Send. | new |
| `ui/label_picker.rs` | Popover for toggling labels (`l`) with a filter input. | new |
| `ui/help.rs` | Keyboard shortcut overlay (`?`). | new |
| `ui/settings.rs` | Settings screen: OAuth client, accounts, preferences. | adapt |
| `ui/auth.rs` | Sign-in, locked and needs-client cards. | split out of antarctical `main_view.rs` |

Delete or never create: `layout.rs`, `calendar_views.rs`, `tray.rs`, `tray_view.rs`,
`reminders.rs`, `dankcal.rs`. **There is no tray in v1.** Closing the last window quits.

### 2.2 Cargo dependencies

Copy antarctical's `[dependencies]` and change them as follows:
- **Drop:** `ksni`, `uuid`, `image`.
- **Keep:** gpui, gpui_platform, async-channel, serde, serde_json, jiff, ureq, url, sha2,
  base64, getrandom, keyring-core, zbus-secret-service-keyring-store, notify-rust, open,
  unicode-segmentation.
- **Add:**
  - `html2text = "0.17"`. Note that `from_read` returns a `Result`.
  - `encoding_rs = "0.8"`
  - `rfc2047-decoder = "1"`
  - `lettre = { version = "0.11", default-features = false, features = ["builder"] }`
- **Dev-dependency:** `mail-parser`, latest; check the API at the time you add it. It is used
  only to round-trip-test the MIME output.

Every non-obvious dependency gets a one-line `#` comment explaining it. Seed `Cargo.lock` with
a copy of antarctical's lock so the gpui tree resolves to the same commit. After that, let
cargo prune it. **Never run a bare `cargo update`**, because it would move the gpui transitive
dependencies. Dependabot is limited to direct cargo dependencies and ignores gpui and
gpui_platform; review every Dependabot lock change against this rule before merging.

`[profile.release]` is `lto = "thin"` and `codegen-units = 1`.

If html2text or lettre fails to build without tokio, or conflicts with the gpui tree, stop and
record the problem in the milestone report. Do not substitute silently. The fallbacks are:
antarctical's `html_to_text` extended with numeric entities, and a hand-written MIME writer
(`text/plain; charset=utf-8`, QP body, RFC 2047 B-encoded headers).

---

## 3. Process and threading model

The model is antarctical's, unchanged:

```
main ─▶ cli::parse ─▶ instance::claim (skipped in demo unless --instance) ─▶ Prefs::load ─▶ worker start
        (demo::start | sync::start_google) -> (SyncHandle, async_channel::Receiver<SyncEvent>)
GPUI app (QuitMode::Explicit):
  store = cx.new(|_| Store::new(sync, mode, prefs))
  cx.spawn: while let Ok(ev) = events.recv().await { store.update(cx, |s, cx| { s.apply(ev); cx.notify() }) }
  cx.spawn: instance actions (Show / Compose(mailto)) -> show_main_window / open_compose_window
  cx.observe(&store): drain store.take_new_mail() -> notify::new_mail_notes -> background_spawn(notify::show)
  on_window_closed: quit when no windows remain;  on_app_quit: send Command::Shutdown
```

- The UI never touches the network or the keyring. UI to worker is `std::sync::mpsc` (wrapped by
  `SyncHandle`); worker to UI is `async_channel::unbounded` with `send_blocking`.
- Browser sign-in runs on a short-lived thread, `eloquawk-sign-in`. It posts
  `Command::SignInFinished(seq, result)` to the worker's own sender (`self_tx`). Copy this from
  antarctical as it is: 5-minute timeout, `AtomicBool` cancel, stale-seq drop.
- `Command` deliberately does not implement `Debug`, because it carries client secrets and
  message bodies. `Outgoing`, `Message` and `ThreadDetail` get hand-written redacting `Debug`
  impls (section 1, "No mail in logs").
- Every `Store` notification re-renders the main window. The reader is therefore its own
  entity, embedded with `.cached(StyleRefinement::default().size_full())`, and it only calls
  `cx.notify()` when the open thread, its detail or its summary changes (section 8.2).

---

## 4. Data model

### 4.1 `model.rs`

```rust
pub type AccountId = String;              // the account's email address; never contains '/'

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadKey { pub account: AccountId, pub id: String }   // Display: "account/id"

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ViewKind { Label(String), AllMail, Search(String) }
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ViewKey { pub account: AccountId, pub kind: ViewKind }

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum LabelKind { System, User }
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub account: AccountId, pub id: String, pub name: String, pub kind: LabelKind,
    pub color: Option<(u32, u32)>,          // (text, background) parsed from "#rrggbb"
    pub threads_unread: u32, pub threads_total: u32, pub messages_total: u32,
    pub hidden: bool,                        // labelListVisibility == "labelHide"
}

#[derive(Clone, Debug, PartialEq)]
pub struct Address { pub name: Option<String>, pub email: String }

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSummary {
    pub key: ThreadKey, pub history_id: u64,
    pub subject: String, pub snippet: String,          // snippet: entity-decoded
    pub participants: Vec<(Address, bool /*unread*/)>, // in message order, deduped by email
    pub last_date: jiff::Timestamp,                    // latest internalDate
    pub message_count: u32, pub label_ids: BTreeSet<String>,
    pub has_attachments: bool,
}
impl ThreadSummary {
    fn unread(&self) -> bool;    // label_ids.contains("UNREAD")
    fn starred(&self) -> bool;
    fn in_label(&self, id: &str) -> bool;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment { pub part_id: String, pub attachment_id: Option<String>,
                        pub filename: String, pub mime_type: String, pub size: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum BodySource { Plain, Html, Empty }
#[derive(Clone, PartialEq)]                        // Debug is hand-written and redacting
pub struct Message {
    pub id: String, pub thread_id: String, pub label_ids: BTreeSet<String>,
    pub from: Address, pub to: Vec<Address>, pub cc: Vec<Address>, pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>, pub date: jiff::Timestamp, pub subject: String,
    pub message_id: Option<String>, pub in_reply_to: Option<String>, pub references: Vec<String>,
    pub snippet: String, pub body: String, pub body_source: BodySource,
    pub body_truncated: bool,                      // input cap hit (section 8.2)
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, PartialEq)]                        // Debug is hand-written and redacting
pub struct ThreadDetail { pub key: ThreadKey, pub subject: String,
                          pub label_ids: BTreeSet<String>, pub messages: Vec<Message> }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LabelDelta { pub add: Vec<String>, pub remove: Vec<String> }
// Helpers: archive(), read(), unread(), star(), unstar(), spam(), not_spam(), inverse()

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ComposeKind { New, Reply, ReplyAll, Forward }
#[derive(Clone)]                                   // Debug is hand-written and redacting
pub struct Outgoing {
    pub account: AccountId, pub kind: ComposeKind, pub from: Address,
    pub to: Vec<Address>, pub cc: Vec<Address>, pub bcc: Vec<Address>,
    pub subject: String, pub body: String,
    pub thread_id: Option<String>, pub in_reply_to: Option<String>, pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
}
#[derive(Clone, Debug)]
pub enum AttachmentSource { Local(PathBuf), Gmail { message_id: String, part_id: String } }
#[derive(Clone, Debug)]
pub struct OutgoingAttachment { pub filename: String, pub mime_type: String, pub size: u64,
                                pub source: AttachmentSource }

pub const MAX_OUTGOING_ATTACHMENTS: u64 = 25 * 1024 * 1024;   // summed `size`, section 8.5
```

System labels shown in the sidebar, in this order (anything not in this table is hidden):

| Label | Shown | Count |
|---|---|---|
| Inbox | yes | `threads_unread` |
| Starred | yes | none |
| Important | yes | none |
| Sent | yes | none |
| Drafts (DRAFT) | yes | `threads_total` |
| All Mail (`ViewKind::AllMail`, not a label) | yes | none |
| Spam | yes | `threads_unread` |
| Trash | yes | none |
| `CATEGORY_*` and CHAT | no | — |

User labels where `!hidden` come after the system labels. They are sorted by name and nested
on `/`, with an indent per level. They show `threads_unread`. A label colour, when present, is
drawn as a dot.

### 4.2 Sender display (`mail/address.rs::participants_label`)

This follows Gmail. The input is a thread's participants and the account's own email; the
output is something like `"Alice, me, Bob (4)"`.
- "me" stands for the account's own address.
- Names are shortened to first names when there are more than two participants.
- Unread participants are marked so the UI can bold them.
- `(n)` is appended when `message_count > 1`.

This logic must be unit-tested.

### 4.3 `Prefs` (`config.rs`, `#[serde(default)]`)

```rust
pub struct Prefs {
    pub theme: ThemePref,               // System | Light | Dark (default System)
    pub clock_24h: bool,                // default true
    pub mark_read_on_open: bool,        // default true
    pub notifications: bool,           // default true
    pub refresh_secs: u64,              // default 60, clamped to 30..=900
    pub page_size: u32,                 // default 50, clamped to 10..=100
    pub last_view: Option<(AccountId, String)>,  // label id; restored at start
    pub collapsed_accounts: BTreeSet<AccountId>,
    #[serde(skip)] pub ephemeral: bool,
}
```

Every settings or store handler that changes a pref calls `save` only when `!ephemeral`.
`save_to` itself returns early when `ephemeral`, so a forgotten check cannot write.
**M3 note:** All Mail is not a label, so `last_view` stores it as the sentinel id
`ALL_MAIL`; search views are not remembered.

---

## 5. Worker protocol (`protocol.rs`)

```rust
pub type OpId = u64;   // allocated by the Store, monotonically increasing

pub enum Command {                                   // NOT Debug
    SaveClient { client_id: String, client_secret: Option<String> },
    ClearClient, AddAccount, CancelSignIn, RemoveAccount(AccountId),
    Refresh,                                         // also retries a locked keyring
    DismissProblems,
    Tune { refresh_secs: u64, page_size: u32 },      // M10: sync prefs changed in Settings
    /// The view the UI is showing; `more` asks for the next page.
    Want { view: ViewKey, more: bool },
    OpenThread(ThreadKey),
    Modify { op: OpId, account: AccountId, thread_ids: Vec<String>, delta: LabelDelta },
    Trash { op: OpId, account: AccountId, thread_ids: Vec<String> },
    /// `restore` is applied with threads.modify after a successful untrash (undo, Restore).
    Untrash { op: OpId, account: AccountId, thread_ids: Vec<String>, restore: LabelDelta },
    Send { op: OpId, outgoing: Outgoing },
    SaveAttachment { account: AccountId, message_id: String, attachment: Attachment },
    Shutdown,
    #[doc(hidden)] SignInFinished(u64, Result<TokenResponse, AuthError>), // google only; demo ignores
}

pub enum AuthState { Starting, Locked, NeedsClient, Ready, Demo }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageMode {
    Replace,   // page 1 of a newly requested view: replaces the list
    Append,    // the next page: appended, deduplicated by id
    Refresh,   // page 1 re-listed by a sync round: merged, keeping the loaded tail (8.1)
}

#[derive(Clone)]                                     // Debug is hand-written and redacting
pub enum SyncEvent {
    Auth(AuthState),
    Client(Option<String>),                          // saved client id (never the secret)
    Accounts(Vec<AccountId>),
    SigningIn(Option<String>),
    Busy(bool), Synced(Timestamp),
    Problem(Option<String>), Notice(String),
    Saved(PathBuf),                                  // M12: an attachment was saved here
    Labels { account: AccountId, labels: Vec<Label> },      // full list: replaces the account's labels
    LabelCounts { account: AccountId, labels: Vec<Label> }, // count refresh: upserts by label id
    ThreadPage { view: ViewKey, ids: Vec<String>, threads: Vec<ThreadSummary>, mode: PageMode, has_more: bool },
    /// Fresh summaries for threads anywhere (history sync, post-mutation). Upsert only.
    ThreadsUpdated(Vec<ThreadSummary>),
    ThreadsGone { account: AccountId, ids: Vec<String> },
    ThreadDetail(ThreadDetail),
    ThreadFailed { key: ThreadKey, error: String },
    MutationFinished { op: OpId, error: Option<String> },
    SendFinished { op: OpId, error: Option<SendError> },
    /// New unread INBOX threads found by an incremental sync (never on the first sync).
    NewMail { account: AccountId, threads: Vec<ThreadSummary> },
    AccountReset(AccountId),                         // history expired, so drop the caches
}
#[derive(Clone, Debug)]
pub struct SendError { pub message: String, pub maybe_sent: bool }
```

The demo emits both `Labels` (at start and on Refresh) and `LabelCounts` (after mutations).

**M3 notes:**
- `ThreadPage` carries `ids`, the page in list order, next to `threads`, the summaries the
  worker already has. Without it a page could not name the ids that 7.3 shows as skeleton
  rows. The store dedupes `ids` and drops any id a pending mutation took out of that view.
- `SignInFinished` carried only the sequence number until `google/oauth.rs` existed; M9
  added the `Result<TokenResponse, AuthError>` payload.
- `Send` holds `Outgoing` by value as written; clippy's `large_enum_variant` did not fire.

Rules:
- The demo worker must handle every `Command` with an exhaustive `match`, so adding a variant
  breaks the demo build.
- `Want` is idempotent. The worker coalesces repeated `Want`s for the same view and answers
  `more` only when it holds a next page token.
- Page tokens never reach the UI.

---

## 6. Google layer

### 6.1 `google/api.rs`

- `agent()` builds a ureq agent: `http_status_as_error(false)`, a default global timeout of
  30 s, user agent `eloquawk/<CARGO_PKG_VERSION>`.
- **Per-request timeouts** (ureq 3 request-level config, `.config().timeout_global(..)`;
  VERIFY the exact builder call): `send`, `attachment` and `thread_full` / `message_full` use
  `30 s + 1 s per 100 KiB` of expected payload (the raw size for send, the attachment `size`
  for downloads, 2 MiB assumed for full fetches), capped at 10 minutes. Everything else keeps
  30 s.
- Endpoints are injectable: `struct Endpoints { base: String, upload: String, token: String }`
  with `Endpoints::google()` returning
  `base = "https://gmail.googleapis.com/gmail/v1/users/me"`,
  `upload = "https://gmail.googleapis.com/upload/gmail/v1/users/me"` and the OAuth token
  endpoint. Tests point all three at a loopback fake server (section 12.2).
- Body limits: `BODY_LIMIT_LARGE = 96 MiB` for full threads and attachments (a 50 MB
  attachment is about 67 MB of base64 JSON), 8 MiB for everything else. Exceeding a limit maps
  to `ApiError::TooLarge`, shown as "Attachment too large to download" or "Message too large to
  open".
- Helpers:
  - `get<T>(agent, token, url, query)`
  - `post_json<B, T>`
  - `post_json_no_content<B>`, which accepts 204 with an empty body
  - `post_empty<T>` (ureq `send_empty()`), for trash and untrash
  - `post_multipart<T>(url, parts)`, for the upload send (`multipart/related`)
- `ApiError` keeps antarctical's variants (`Unauthorized`, `Forbidden(String)`, `NotFound`,
  `Network(NetKind)`, `Http{status, body}`, `Decode`, `InvalidRequest`) and adds:
  - `RateLimited { retry_after: Option<Duration> }`
  - `QuotaExhausted { message: String, retry_after: Option<Duration> }`, for daily limits
    and sending limits (M10 review note: this was `QuotaExhausted(String)`, which lost the
    hint of a converted long rate limit, so the worker waited 15 minutes instead of the
    hinted time)
  - `TooLarge`
  `Network` holds only an error kind (timeout, connect, tls, io, other) and never the URL.
- Error classification:
  - 429 is `RateLimited`.
  - For 403, lowercase the body first. A body containing `ratelimitexceeded` (this catches
    `userRateLimitExceeded`) is `RateLimited`. A body containing `dailylimitexceeded` or a
    sending-limit message is `QuotaExhausted`.
  - 500, 502, 503 and 504 are `Http`, and retryable.
  - **Retry hints:** `Retry-After` is parsed as either delta-seconds or an HTTP-date. When the
    header is absent, the error envelope message is searched (lowercased) for
    `retry after <RFC 3339 timestamp>` and the difference from now is used. A `RateLimited`
    whose `retry_after` exceeds 15 minutes is converted to `QuotaExhausted` with the Google
    message.
- Keep `error_message()` and `missing_scope()`.

### 6.2 `google/gmail.rs` (DTOs use `#[serde(rename_all = "camelCase")]`)

- Every array field gets `#[serde(default)]`.
- Ids, `historyId` and `internalDate` are JSON strings, parsed as `u64` / `i64` in
  `mail/parse.rs`.
- Every function takes `&Endpoints` (or the base string) so tests can redirect it.

| Function | Request | Units |
|---|---|---|
| `profile()` | `GET /profile` returns `{emailAddress, historyId}` | 1 |
| `labels()` | `GET /labels` | 1 |
| `label(id)` | `GET /labels/{id}`, for counts | 1 each |
| `threads(view, page_token, max)` | `GET /threads?labelIds=..&q=..&maxResults=..&pageToken=..`. `Label(id)` sets labelIds; `AllMail` sets neither; `Search(q)` sets `q`. `Label("TRASH")` and `Label("SPAM")` also set `includeSpamTrash=true`. Returns `{threads[{id,snippet,historyId}], nextPageToken}`. | 10 |
| `thread_meta(id)` | `GET /threads/{id}?format=metadata` with `metadataHeaders` From, To, Cc, Subject, Date | 10 |
| `thread_full(id)` | `GET /threads/{id}?format=full` | 10 |
| `modify_thread(id, delta)` | `POST /threads/{id}/modify` with `{addLabelIds, removeLabelIds}`, once per thread. This is the only thread mutation path: it applies to every message in the thread server-side, including ones that arrived after our last fetch. | 10 each |
| `modify_message(id, delta)` | `POST /messages/{id}/modify`. Used only to star: Gmail web stars the latest message, not the whole thread. | 5 |
| `trash(id)` / `untrash(id)` | `POST /threads/{id}/trash` / `/untrash`, with an empty body | 10 each |
| `send(raw_bytes, thread_id)` | Always `POST UPLOAD/messages/send?uploadType=multipart` with `Content-Type: multipart/related`: part 1 `application/json; charset=UTF-8` `{"threadId": ..}` (or `{}`), part 2 `message/rfc822` with the raw bytes. This carries `threadId` at any size. VERIFY the response shape matches `messages.send`. | 100 |
| `attachment(msg, att)` | `GET /messages/{m}/attachments/{a}` returns `{data, size}` | 5 |
| `message_full(id)` | `GET /messages/{id}?format=full`, used to refresh attachmentIds before a download | 5 |
| `history(start, page)` | `GET /history?startHistoryId=..&historyTypes=messageAdded&historyTypes=messageDeleted&historyTypes=labelAdded&historyTypes=labelRemoved&maxResults=500`. A 404 maps to `HistoryExpired`. | 2 |

`messages/batchModify` is not used. The worker keeps each thread's message ids (from the
metadata fetch) only to pick the star target: the last message that is not a draft. If the
thread is not in `known`, star first calls `thread_meta`. A thread that holds only drafts has
no such message, so it is starred with `modify_thread` adding STARRED instead.

The Gmail JSON fixtures used in tests live in `src/google/fixtures/*.json`, loaded with
`include_str!`. Write them by hand from the documented shapes, and mark any that are unverified.

### 6.3 `google/oauth.rs`

Copy antarctical's file and change the following:
- **Scope:** `SCOPES = &["https://www.googleapis.com/auth/gmail.modify"]`.
- **Drop:** all RSVP and reply-scope code.
- **Client:** `builtin_client() -> Option<ClientCredentials>`: the compiled-in Eloquawk
  Desktop client, with `ELOQUAWK_GOOGLE_CLIENT_ID` / `_SECRET` (compile time) overriding each
  part. It returns `None` only when the id or the secret ends up empty (section 1).
- **Unchanged:**
  - `access_type=offline`, `prompt="select_account consent"` and `login_hint` on re-auth
  - The redacted `Debug` impls
  - The tests (RFC 7636 vector, state match, cancel)
- **Strings:** the loopback page says "Eloquawk is connected to Gmail. You can close this tab."
- **Account email:** from `gmail::profile().email_address`. Do not use the calendar call.
- **Granular consent:** in `finish_sign_in`, before calling `profile` or saving anything, check
  that the space-separated `tokens.scope` contains
  `https://www.googleapis.com/auth/gmail.modify`. If not, show the missing-scope message from
  7.6 and save nothing. Revoke the new grant only if the account is not already known; a
  known account's existing token is never revoked here.

### 6.4 `secrets.rs`

Copy antarctical's file and change the following:
- `SERVICE = "korora.tech.Eloquawk"`
- Labels read `Eloquawk: OAuth client` and `Eloquawk: Google accounts`.
- Delete everything legacy.
- Refresh tokens stay a single JSON map item, `email → refresh_token`.
- `#[cfg(debug_assertions)]`: when `ELOQUAWK_SECRETS=memory`, use a `MemoryStore` instead of
  the keyring, optionally seeded from `ELOQUAWK_SECRETS_SEED=<path to JSON>`. Release builds
  ignore both variables. This lets M9 and M13 be verified without touching the user's keyring.

---

## 7. Sync algorithm (`sync.rs`)

### 7.1 Worker state

```rust
struct Worker {
    events: Sender<SyncEvent>, self_tx: mpsc::Sender<Command>, agent: ureq::Agent,
    endpoints: Endpoints,
    secrets: Secrets, client: Option<ClientCredentials>, auth: AuthState,
    accounts: BTreeMap<AccountId, AccountState>, sign_in: Option<SignIn>,
    problems: BTreeMap<String, String>,        // sticky; keys: email | "client" | "tokens" | "network"
    jobs: VecDeque<Job>,                       // two lanes, see 7.2
    want: Option<ViewKey>,                     // the view the UI shows
    next_history_poll: Instant,
}
struct AccountState {
    refresh_token: String, access: Option<(String, Instant)>,
    history_id: Option<u64>, synced_once: bool,
    known: HashMap<String, (u64 /*history id*/, Vec<String> /*message ids*/, ThreadSummary)>,
    pages: HashMap<ViewKind, Option<String>>,  // next page token per view
    pending_changed: BTreeSet<String>,         // thread ids whose refetch has not succeeded yet
    pending_fresh: BTreeSet<String>,           // candidate NewMail thread ids not yet reported
    missing: BTreeSet<String>,                 // listed ids still without a summary (skeletons)
    deferred: Vec<Job>,                        // background jobs parked while in backoff
    backoff: Backoff,                          // attempts, retry_at
}
```

### 7.2 Job queue and loop

```rust
enum Job {
    // interactive lane: front of the queue, run in FIFO order among themselves
    OpenThread(ThreadKey), Mutate(Mutation), Send(OpId, Outgoing), Save(Download), ListView(ViewKey, bool),
    // background lane: back of the queue, coalesced (no duplicate (variant, account, ids))
    InitialSync(AccountId), Labels(AccountId, Vec<String> /*label ids, ≤10*/),
    FetchMeta(AccountId, Vec<String> /*≤10*/), History(AccountId), Refetch(AccountId),
}
```

The loop runs like this:
1. Block on `recv_timeout(d)`. `d` is `0` while jobs are queued; otherwise it is the time until
   the next history poll or the earliest `retry_at`, capped at 30 s.
2. Drain every queued command with `try_recv`.
   - Commands that need HTTP enqueue a job. Interactive jobs go to the front; background jobs go
     to the back.
   - Account and client commands run inline, as in antarctical.
3. Re-queue `deferred` jobs of every account whose `retry_at` has passed.
4. Pop **one** job and run it. A background job for an account still in backoff is moved to
   that account's `deferred` (coalesced) instead of running. Interactive jobs always run.
   - A long `FetchMeta`, `Refetch` or label-count refresh is split into chunks of 10 ids. Each
     chunk is its own job, so an `OpenThread` waits for at most one chunk.
   - Each chunk emits `ThreadsUpdated`, `ThreadPage` or `LabelCounts` immediately, so rows appear as
     they load.
5. When the history poll is due, for every account whose `retry_at` has passed:
   - queue `InitialSync(account)` if `history_id.is_none() || !synced_once`, otherwise
     `History(account)`;
   - queue `FetchMeta` for any ids in `missing` (skeleton rows left by a failed chunk).
   Then set the next poll for `prefs.refresh_secs` later. The worker receives that value
   (and `page_size`) at start and again through `Command::Tune`, which the Store sends
   whenever Settings changes either. The demo ignores the interval and applies the page size.
   (M10 note: this said "through `Refresh` / `SaveClient`", which carry no prefs.)
6. Only one HTTP request is ever in flight. This avoids Gmail's "too many concurrent requests".

`Busy(true)` is emitted when a `ListView`, `InitialSync` or `History` job starts with the
interactive lane empty. `Busy(false)` is emitted when both lanes drain. `Synced(now)` is emitted
after every successful history round.

### 7.3 Initial sync, per account

This runs after the keyring loads, after a sign-in finishes, after `Refresh`, after
`AccountReset`, and from the poll while the account has not completed one (7.2 step 5).
1. `profile()`, then `history_id = profile.historyId`. This is recorded **before** listing, so no
   change can be missed.
2. `labels()`, emit `Labels` (the full list), then queue the per-label `label(id)` count
   refresh for every visible label as chunked `Labels` jobs, each emitting `LabelCounts`.
3. List INBOX page 1, and also `want`'s view if it differs and belongs to this account.
   - For each returned `{id, historyId}`, if `known[id].history_id == historyId`, reuse the
     cached summary; otherwise add it to `missing` and queue it for `FetchMeta`.
   - Emit `ThreadPage { mode: Replace }` straight away with the cached summaries in list order.
     The ids still pending show as skeleton rows (the store shows ids it has no summary for as
     skeletons).
   - Then let the `FetchMeta` chunks fill the rest in, removing ids from `missing` as they land.
4. Set `synced_once = true`. The first sync **never** emits `NewMail`.

Any failure aborts the job with the 7.6 handling; `synced_once` stays false, so the next poll
queues `InitialSync` again.

### 7.4 Incremental sync (`History(account)`)

Only queued when `history_id.is_some() && synced_once`.
1. Page through `history(start = history_id)` into a pure `collect_changes(&[HistoryPage]) ->
   Changes`:
   - `changed: BTreeSet<threadId>` from every `messagesAdded`, `messagesDeleted`,
     `labelsAdded` and `labelsRemoved`
   - `fresh_inbox: BTreeSet<threadId>` from `messagesAdded` whose labelIds contain INBOX and
     UNREAD, and contain none of SENT, DRAFT, SPAM, `CATEGORY_PROMOTIONS`, `CATEGORY_SOCIAL`,
     `CATEGORY_UPDATES` or `CATEGORY_FORUMS` (Gmail's own notification behaviour with
     categories on; documented in docs/google-gmail.md)
   - `new_history_id` from the last page.
   If any history page fails, the round aborts and nothing is committed.
2. **Commit the cursor together with the work it implies**, in one step:
   `pending_changed ∪= changed`, `pending_fresh ∪= fresh_inbox`,
   `history_id = new_history_id`. The pending sets survive failures, so advancing the cursor
   can never lose a change.
3. If `pending_changed` is empty, emit `Synced` and stop: no further calls.
4. Queue `Refetch(account)`. Each `Refetch` chunk takes up to 10 ids from `pending_changed`
   (ids also in `pending_fresh` first):
   - `thread_meta(id)`: on success, update `known`, emit `ThreadsUpdated`, remove the id from
     `pending_changed`.
   - On 404, emit `ThreadsGone`, drop the id from `known`, remove it from `pending_changed`
     and `pending_fresh`.
   - Any other error stops the chunk with the 7.6 handling. Unprocessed ids stay pending and
     are retried by the next round.
   - **Large bursts:** if `pending_changed` holds more than 100 ids, refetch only the ids in
     `pending_fresh` (at most 20, newest first); for the rest, drop their `known` entries (so
     any later listing refetches them) and clear them from `pending_changed`.
5. When `pending_changed` becomes empty, re-list page 1 of INBOX and of `want`'s view and emit
   `ThreadPage { mode: Refresh }`. A refresh never touches `pages[view]`.
6. Refresh label counts for the labels the changed threads carried before or after, plus
   INBOX, SPAM and DRAFT (chunked `Labels` jobs). Emit `Synced`.
7. NewMail: emit `NewMail { threads }` where `threads` are the `pending_fresh` ids whose
   refetched summaries are still unread and in INBOX; then clear those ids from
   `pending_fresh`. Ids that were refetched but no longer qualify are cleared without a
   notification.
8. **HistoryExpired (404):** emit `AccountReset(account)`, clear `known`, `pages`,
   `history_id`, `pending_changed`, `pending_fresh` and `missing`, set `synced_once = false`,
   then queue `InitialSync`. No `NewMail` is emitted for that round.

The cursor/pending bookkeeping lives in pure methods on `AccountState`
(`commit_round(changes)`, `refetch_succeeded(id)`, `refetch_gone(id)`) so it is unit-tested
without HTTP.

### 7.5 Interactive jobs

- **`ListView(view, more)`:** page 1 or the next page, with `page_size` threads. Cached summaries
  are reused and the rest are fetched with `FetchMeta`, as in 7.3. Page 1 emits
  `mode: Replace`, a next page `mode: Append`. `Want` updates `want` and queues this job.
- **`OpenThread(key)`:** `thread_full`, then `mail/parse`. Emits `ThreadDetail` or `ThreadFailed`.
  This job never marks the thread read; the Store decides that and sends `Modify`.
- **`Mutate`:**
  - `Modify`: if the delta is exactly `add [STARRED]`, star the last non-draft message with
    `modify_message`; otherwise call `modify_thread` for each thread (unstar is
    `modify_thread` removing STARRED, which clears it from every message).
  - `Trash` / `Untrash`: `trash` / `untrash` per thread. After a successful untrash, apply
    `restore` with `modify_thread` when it is non-empty.
  - Then emit `MutationFinished { op, error }`. A failure part-way reports the error; threads
    already changed are corrected by the refetch below.
  - Afterwards, refetch those threads' metadata and emit `ThreadsUpdated` (the authoritative
    state), then refresh the counts of the labels in the delta plus INBOX, SPAM, TRASH, DRAFT
    and any label the touched threads carry (chunked `Labels` jobs). Never a full label
    refresh.
  - The worker does not retry a mutation automatically; on failure the store rolls back. The one
    exception is `Unauthorized`, which gets the single token-refresh retry of `with_token`.
- **`Send(op, out)`:**
  1. Resolve `AttachmentSource::Gmail` parts: `message_full`, find the part by `part_id`, then
     `attachment` (or the part's inline `body.data` when it has no `attachmentId`). Until M12
     this path returns `SendError { maybe_sent: false, message: "Forwarding attachments isn't
     available yet." }`.
  2. Read `Local` files with `std::fs::File::open(..).take(MAX_OUTGOING_ATTACHMENTS + 1)`. If
     the summed bytes exceed `MAX_OUTGOING_ATTACHMENTS`, return
     `SendError { maybe_sent: false, message: "Attachments are larger than 25 MB." }`.
  3. Build the bytes with `mail/compose.rs`, then call `send` (multipart upload, 6.2) with the
     size-scaled timeout.
  - **Never retry.** Classification of failures:
    - `Network` (any kind, including timeouts) and any 5xx: `maybe_sent: true`, message
      "Couldn't confirm the message was sent. Check Sent before trying again."
    - Any 4xx (400, 403 quota, 401 after the single refresh retry, 413) and local errors
      (file read, MIME build): `maybe_sent: false`, with the Google or local message.
  - On success, emit `SendFinished { error: None }` and queue `History(account)`.
- **`SaveAttachment`** (M12):
  1. `message_full(message_id)`, then find the part by `part_id` to get a fresh `attachmentId`,
     because those ids are not stable. If the part has inline `body.data` and no
     `attachmentId`, use that data and skip step 2.
  2. Call `attachment`.
  3. Resolve the download directory (`mail/files.rs`, pure): read `XDG_DOWNLOAD_DIR` from
     `$XDG_CONFIG_HOME/user-dirs.dirs`, expand only a leading `$HOME`, and require the result to
     be absolute; otherwise use `~/Downloads`. `create_dir_all` it.
  4. Sanitise the filename: strip `/`, `\`, NUL, C0 and C1 controls, and bidi or invisible
     formatting characters (U+200E, U+200F, U+202A–U+202E, U+2066–U+2069); trim leading dots
     and spaces; map `""`, `.` and `..` to `attachment`; truncate to 200 bytes on a char
     boundary while keeping the extension.
  5. Never overwrite: try `name.ext`, then `name (1).ext` and so on, each with `create_new`.
     On a write error, remove the partial file.
  6. Emit `Notice("Saved <file> to <dir>")`; the toast offers "Open folder".
     (M12 change: the worker emits `SyncEvent::Saved(path)` instead, and the store posts
     "Saved <file> to <dir>" (home shown as `~`) with `NoticeAction::OpenFolder(path)`. A
     plain `Notice` cannot carry the path the button needs. "Open folder" calls gpui's
     `cx.reveal_path`, which uses the portal's OpenDirectory with the file selected, or
     xdg-open on the folder.)

### 7.6 Tokens, accounts and errors

**Tokens and accounts.** Copy antarctical's methods: `load_from_keyring`, `save_client`,
`begin_sign_in`, `cancel_sign_in`, `finish_sign_in` (with the scope check of 6.3 and the email
taken from `profile`), `access_token` (refreshed when less than 60 s remain), `with_token`
(retry once on 401), `remove_account` and `remove_all_accounts` (never revoke a token whose
account is unknown), `report` and the sticky problems. Changes from antarctical:
- `access_token` maps `AuthError::Network` (token endpoint unreachable) to the same handling
  as `ApiError::Network`: backoff and the `"network"` problem. `AuthError::Other` with a 5xx
  is treated the same way. Only `InvalidGrant` / `InvalidClient` remove accounts or the client.
- `invalid_grant` on refresh removes the account locally and leaves the sticky message
  "Google signed <email> out. Add it again." followed by "(Test-mode tokens expire after 7
  days.)" only when the error description does not mention `invalid_rapt` / reauth; for
  `invalid_rapt` the hint is "(Your organisation requires you to sign in again.)".
- With no saved client and no built-in client, emit `Auth(NeedsClient)`.

**Backoff, per account.**

| Error | Action |
|---|---|
| `RateLimited`, `Network`, `Http` 5xx, `Decode`, token-endpoint network failures | `retry_at = now + max(retry_after, min(5s·2^attempts, 300s)) + jitter(0..1000ms)`. `attempts` resets on success. The account's background jobs are **deferred** (parked in `deferred`, re-queued when `retry_at` passes), never dropped. Sets a sticky problem: `"network"` for network failures, otherwise the account. |
| `QuotaExhausted` (including `RateLimited` with `retry_after` > 15 min) | Sticky problem with the Google message. `retry_at` is the hinted time, or 15 minutes, whichever is later; no retry before then except a manual Refresh. |
| `Forbidden`, `NotFound`, `Http` 4xx on one thread or label | Collected as a problem; sync carries on. |
| `TooLarge` | Reported on the job (`ThreadFailed` or a notice); no backoff. |
| `missing_scope` | Sticky "Eloquawk needs permission to read and organise mail for <email>. Add the account again." |

On the first success after a network failure, clear the `"network"` problem.

Jitter comes from `getrandom`. `Backoff` is a pure struct with an injectable `now`, and it is
unit-tested.

---

## 8. Store and UI

### 8.1 `Store`

```rust
pub struct Store {
    pub mode: Mode, sync: SyncHandle, pub prefs: Prefs,
    pub auth: AuthState, pub client_id: Option<String>, pub accounts: Vec<AccountId>,
    pub signing_in: Option<String>, pub busy: bool, pub last_sync: Option<Timestamp>,
    pub problem: Option<String>, pub notice: Option<Notice>,      // text, action, posted_at
    pub labels: HashMap<AccountId, Vec<Label>>,
    pub threads: HashMap<ThreadKey, ThreadSummary>,
    pub lists: HashMap<ViewKey, ListState>,        // ListState { ids: Vec<String>, has_more, loading }
    pub view: Option<ViewKey>,                     // current view
    pub cursor: Option<ThreadKey>,                 // highlighted row, by key, never by index
    pub selected: BTreeSet<ThreadKey>,             // x-selection
    pub open: Option<ThreadKey>,                   // thread in the reading pane
    pub details: HashMap<ThreadKey, DetailState>,  // Loading | Loaded(ThreadDetail) | Failed(String); keep the 30 most recent
    pending: BTreeMap<OpId, Pending>,              // optimistic mutations
    undo: Option<(OpId, UndoAction)>,              // the last undoable mutation
    sends: HashMap<OpId, SendState>,
    next_op: OpId, new_mail: Vec<ThreadSummary>,   // drained by main for notifications
}
```

The whole Store, including mutations, rollback, undo, auto-advance, selection and page
merging, is built in M3. Later milestones only wire UI to it.

Queries are pure and tested:
- `visible_rows()` returns the current list ids mapped to summaries, or a skeleton when the
  summary is missing.
- `labels_for(account)` returns them sorted and nested.
- `unread_badge(account, label)`.
- `cursor_index()`.
- `participants_label(..)`.

Navigation:
- `set_view(view)` sends `Want`, resets the cursor to the first row, and persists `last_view`.
- `move_cursor(±1)` clamps to the list. Moving into the last 10 rows while `has_more` sends
  `Want { more: true }`.
- `open_cursor()` sets `open` and sends `OpenThread` unless the detail is cached and newer. If
  `mark_read_on_open` and the thread is unread, it calls `mutate(read)` (not undoable, no
  toast).

**Page merging** (`ThreadPage`):
- `Replace`: the list becomes the page's ids.
- `Append`: ids not already in the list are appended (dedupe on every append).
- `Refresh`: the new page replaces the old list's prefix up to and including the old position
  of the page's last id; the old tail after that position is kept, minus any id now in the
  page. If the page's last id is not in the old list, the page replaces the first
  `page.len()` old ids and the rest is kept, deduplicated. `has_more` is only updated when the
  list held a single page.
- The cursor and open thread are keys, so they survive merges. If the cursor's key left the
  list, the cursor moves to the row at its old index, clamped.

**Optimistic mutations** (adapted from antarctical's `pending_replies`):
- `mutate(targets, MutationKind)` allocates an op. `MutationKind` is
  `Labels(LabelDelta) | Trash | Untrash { restore: LabelDelta }`.
- Targets are the selection when it is non-empty, otherwise the open thread, otherwise the
  cursor row.
- It snapshots `Pending { kind, before: Vec<(ThreadKey, ThreadSummary, Vec<(ViewKey, index)>)> }`.
- It applies the delta to `threads` and the label counts straight away. Trash is modelled as
  adding TRASH and removing INBOX (VERIFY item 8; the authoritative refetch corrects it).
- It removes the threads from every list they no longer belong to:
  - `Label(L)` loses a thread when L was removed; when TRASH was added and L ≠ TRASH; or when
    SPAM was added and L ≠ SPAM.
  - `AllMail` loses a thread when TRASH or SPAM was added.
  - `Search(q)` loses a thread when TRASH or SPAM was added, unless the query asks for them
    (`in:trash`, `in:spam`, `in:anywhere` or the matching `label:`), as Gmail leaves them out
    of results; other label changes leave search results alone until the search runs again.
    (Note, M7 review: originally search lists were always left alone, which kept trashed
    threads in the results.)
- If the open thread was removed, the next row opens; failing that, the previous one; failing
  that, the reader closes. This is the Gmail "auto-advance to older" behaviour.
- It sends the `Command`.
- `MutationFinished { error: None }` drops the pending op.
- `MutationFinished { error: Some }` restores the summaries and list positions and posts the
  notice `"Couldn't <verb>: <error>"`.
- While an op is pending, incoming `ThreadsUpdated` / `ThreadPage` data for its threads has the
  pending delta re-applied, so the UI does not flicker back.
- **Undo:** archive, trash, spam, not spam, restore and label changes post an undoable notice,
  e.g. "Archived 1 conversation · Undo". Undo is a new op:
  - for a label delta, the inverse delta;
  - for trash, `Untrash { restore }` where `restore` adds back every label the snapshot had
    (INBOX included) that untrash may not restore;
  - for untrash (Restore), `Trash`.
  `z` triggers the same undo while the notice is showing.

**Event handling:**
- `ThreadPage` upserts summaries, then merges as above.
- `Labels` replaces that account's labels; `LabelCounts` upserts counts by label id.
- `AccountReset` drops the account's threads, lists and details.
- `Accounts` prunes data for removed accounts. When the current view is unset or belongs to a
  removed account, it picks `last_view` or the first account's INBOX.
- `NewMail` pushes onto `new_mail` only when `prefs.notifications` is on.

### 8.2 Layout (`ui/main_view.rs`)

```
┌ header (HEADER_H 56): icon slot · search input (flex) · busy dot · refresh · settings ┐
├ banner (problem, dismiss ✕) when present                                              ┤
│ sidebar SIDEBAR_W 240 │ thread list LIST_W 420 (flex_shrink_0) │ reader flex_1 min_w 0 │
│  [Compose]            │  view title + count + "Select" hints   │  action bar           │
│  ▾ you@example.com    │  uniform_list rows (ROW_H 68)           │  subject + labels      │
│    Inbox        12    │  …                                      │  message cards…        │
│    Starred            │  (sibling below the list)               │                        │
│  ▸ you@work.example 3 │  footer: Loading… / Load more           │  empty: "No conversation│
│  footer: account box  │                                         │   selected"            │
└──────────────────────────────────────────────────────────────────────────── toast ──┘
```

- **Screens:** `Screen::Mail`, `Screen::Settings` and `Screen::Auth`. Auth is shown when there
  are no accounts, or when the keyring is locked or `NeedsClient`; it uses the auth cards from
  antarctical.
- **Window:** 1280×820, minimum 960×620, title "Eloquawk".
- **Header icon:** M4 reserves a fixed 28×28 slot (empty). M14 fills it with
  `branding::icon(28)`.
- **Account sections:** the active account is expanded. Collapsed accounts show their inbox
  unread total on the header row. Clicking a header toggles it, and the state persists in
  `collapsed_accounts`.
- **Thread list:** a flex column holding the `uniform_list` (flex_1) and, as a sibling below it,
  the footer ("Loading…" or a "Load more" button). The list tracks a
  `UniformListScrollHandle`; every cursor move calls `scroll_to_item(ix, ScrollStrategy::..)`
  with the strategy that keeps the row visible with the least movement (**M4:** that is
  `ScrollStrategy::Nearest`; the main view scrolls whenever the cursor key changed since its
  last render, so auto-advance and clicks follow too). When the
  visible range reaches the last 10 rows and `has_more`, the render callback asks for more
  through `cx.processor` (the store dedupes the request).
- **Thread row** (fixed `ROW_H`, with explicit `line_height` on every text line):
  - Line 1: unread dot, then the participants label (semibold in the text colour when unread,
    regular in the secondary colour when read, `truncate`), with the date right-aligned and
    tabular.
  - Line 2: the subject (styled like the participants label), then " — " and the snippet in
    muted text, all `truncate`d. On the right, a star (★ in AMBER or ☆ muted, and clicking it
    toggles), plus an attachment mark when `has_attachments` (a paper clip drawn with divs; the
    UI font has no clip glyph).
  - Row states: hover, cursor (SELECTED background with an accent left border 3px), x-selected
    (a checkbox square filled), and skeleton (grey bars).
- **Reader (`ReaderView` entity, `ui/reader.rs`):**
  - Embedded with `.cached(StyleRefinement::default().size_full())`. It observes the Store and
    notifies only when `open`, the open thread's detail or summary, or the theme changes.
  - One `overflow_y_scroll` div with its own `ScrollHandle`. Its children are: child 0 the
    subject and label chips, then exactly one child per message card (card i is child i + 1),
    so `scroll_to_item(i + 1)` brings card i into view. When the open thread changes, the
    offset is reset with `set_offset(point(px(0.), px(0.)))`.
  - The action bar has: Archive, Trash, Mark unread, Star, Labels…, Reply, Reply all, Forward,
    Open in Gmail. In the Trash view, Trash is replaced by **Restore** (`Untrash { restore:
    add INBOX }`); in the Spam view, Archive is replaced by **Not spam** (`+INBOX −SPAM`).
  - Open in Gmail: `open::that(format!("https://mail.google.com/mail/u/?authuser={}#all/{}", ..))`
    with the email percent-encoded by `url::form_urlencoded::byte_serialize`.
  - Under the subject title, label chips (user labels only, coloured).
  - Each message is a card.
    - Collapsed cards show the sender, the snippet and the date in one line.
    - Expanded cards show the sender name and address, "to …" details, the date, a "Copy"
      ghost button, then the body, then the attachments.
    - By default, unread messages and the last message are expanded. Click a header to toggle.
      `;` expands all and `:` collapses all.
  - **Body rendering: selectable text.** gpui at this rev has no selectable static text
    (`StyledText` / `InteractiveText` have no selection), so Eloquawk builds its own:
    - `ui/text_layout.rs` is a shared core used by both `BodyText` and `TextArea`. It lays out
      with `window.request_measured_layout` and a measure closure that calls
      `shape_text(text, font_size, runs, Some(width), None)`, so height follows width. It
      converts `WrapBoundary { run_ix, glyph_ix }` to byte offsets through
      `unwrapped_layout.runs[r].glyphs[g].index`, maps positions to byte offsets with
      `index_for_position` / `position_for_index`, and paints selection rectangles and link
      underlines.
    - `ui/body_text.rs`: a `BodyText` entity per expanded message (the reader keeps
      `HashMap<message_id, Entity<BodyText>>`, dropped when the thread closes). It owns the
      text, the link ranges, the selection and a focus handle with key context
      `ReadOnlyText`. Mouse down/drag/up selects (double-click selects a word); ctrl-c copies
      the selection, ctrl-a selects the whole body, both bound in `ReadOnlyText`. Copy uses
      `cx.write_to_clipboard(ClipboardItem::new_string(..))`.
    - The card's "Copy" button copies the whole visible body even with nothing selected.
    - Selection spans one message body; it does not cross cards.
    - Paragraphs are separated by blank lines inside the one text, not separate divs.
    - A trailing block of lines starting with `>`, or an "On … wrote:" line followed by quoted
      lines, folds behind a "• • •" toggle. When unfolded, the quoted part is a second
      `BodyText` below the first.
  - **Links:** `mail/links.rs::find_links` detects `http://`, `https://` and `mailto:` URLs and
    bare `www.` hosts (opened as https) in the body text. `BodyText` underlines them in the
    accent colour and shows a pointer cursor. A click (mouse up without a drag) on a link
    opens it: http and https through `open::that`; mailto opens the compose window with the
    whitelisted prefill of `mail/mailto.rs`. No other scheme is ever opened (no `file:`,
    `javascript:` or custom handlers). Hit-testing lives in `BodyText`, sharing the selection
    hit-test.
  - **HTML bodies** are converted in `mail/html.rs` on the worker:
    - html2text with a small custom decorator (`InlineLinks`) that emits no emphasis or
      heading markers, and renders an anchor as `text <href>` when the text differs from the
      href, or as the bare href when they match. This keeps the real target visible, so
      phishing links are obvious, without footnote clutter. VERIFY the 0.17 decorator API; if
      a built-in config does exactly this, use it.
    - The HTML input is capped at 2 MiB (truncated on a char boundary before parsing), and
      `html_to_text` runs inside `std::panic::catch_unwind`; on panic the body falls back to the
      snippet. Either case sets `body_truncated`.
    - Remote images are never fetched.
    - `BodySource::Html` shows a small "Converted from HTML · Open in Gmail" caption.
  - **Large bodies:** a body over 200 KB of text is cut on a char boundary in the reader, with a
    "Message truncated · Open in Gmail" caption. `body_truncated` shows the same caption.
  - **Attachments:** a row per file with the name, the human-readable size and a "Save"
    button. M4 renders the Save button disabled; M12 enables it to send `SaveAttachment`. In
    demo mode Save posts the notice "Demo mode doesn't save files".
- **Toast:** at the bottom right, with an optional action ("Undo", "Open folder"). It expires
  after 5 s via a **single stored** `Task`; do not spawn a timer each frame.

### 8.3 Theme (`ui/theme.rs`)

- `pub struct Palette { bg, panel, panel_raised, header, sidebar, border, text, secondary, muted,
  caption, disabled, accent, accent_text, on_accent, green, red, amber, selected,
  selected_border, hover, selection, input_border, skeleton, link }` (all u32).
- `DARK` is antarctical's tokens. `LIGHT` is a new palette: bg f7f7fb, panel ffffff, text 1b1d26,
  and an accent close to 6a52e0. Tune it until the contrast tests pass.
- `struct Theme(&'static Palette)` is a `Global`. `palette(cx: &App) -> &'static Palette`.
- On start, and from `cx.observe_window_appearance` on the main window, resolve `prefs.theme`
  (System follows `window.appearance()`), set the global, and call `cx.refresh_windows()`.
- The `--theme` flag overrides the pref for the session and is not saved.
- **UI font:** gpui's Linux default family is "IBM Plex Sans". When it is not installed,
  cosmic-text falls back to a single face and every `font_weight` renders regular (seen in
  the M4/M5 shots: unread rows looked like read ones). At start `theme::init_ui_font` picks
  the first installed family of IBM Plex Sans, Inter, Cantarell, Noto Sans, Adwaita Sans,
  DejaVu Sans and Liberation Sans, and each window root sets it with `font_family`.
  **M14:** the Nix package's wrapper sets a default `FONTCONFIG_FILE` that includes
  `/etc/fonts/fonts.conf` and the user's `fontconfig/fonts.conf` and adds `pkgs.inter`'s font
  directory, so a Nix install always has a family with real weights. gpui's fontdb only scans
  directories named by fontconfig, so prefixing `XDG_DATA_DIRS` would not help. A
  `FONTCONFIG_FILE` the user already set wins. Non-Nix installs still depend on installed
  fonts.
- **Tests:** WCAG ratio ≥ 4.5 for text, secondary, accent_text and link on bg, panel and
  selected, and ≥ 3.0 for muted and caption, in both palettes.

### 8.4 Settings (`ui/settings.rs`)

There are three cards:
- **OAuth client:** client id field and masked secret field (with Show/Hide), with Save and
  "Use built-in client" (only when a client is saved and the build has a built-in one). Includes a link to docs/google-gmail.md and to
  `https://console.cloud.google.com/apis/library/gmail.googleapis.com`.
- **Accounts:** a list with Remove, plus Add account.
- **Preferences:**
  - Theme as a segment control: System, Light, Dark
  - 24-hour clock
  - Mark as read when opened
  - Notify about new mail
  - Refresh interval segment: 1, 5 or 15 min

Escape or ✕ closes Settings. Toggle handlers update `store.prefs` and call `save` only when
not ephemeral.

### 8.5 Compose window (`ui/compose.rs`)

- Opens as a separate window (`open_compose_window(Outgoing prefill)`, 720×640, title
  "New message" / "Re: …" / "Fwd: …").
- **Initial focus:** `open_compose_window` focuses To for a new message or a forward, and the
  body for a reply or reply-all.
- Fields:
  - From: a selector when there are several accounts. It defaults to the current account; for a
    reply, the account the thread belongs to.
  - To, Cc and Bcc as `TextInput`s. Cc and Bcc are hidden behind a "Cc Bcc" link until used.
  - Subject.
  - Body as a `TextArea`.
  - Attachment chips with remove ✕.
  - An "Attach…" button using `cx.prompt_for_paths`. This is compiled in with the spec's
    features (gpui_linux implements it through the ashpd xdg FileChooser portal). If the
    receiver yields `Err` at runtime (no portal), post an inline error "Couldn't open the file
    chooser" and leave the button enabled.
  - At attach time, read each file's size with `std::fs::metadata`. If the summed size would
    exceed `MAX_OUTGOING_ATTACHMENTS` (25 MB), reject the file with the inline error
    "Attachments can't exceed 25 MB in total". The worker re-checks the bytes it reads (7.5).
- Address fields parse with `mail/address.rs`. Invalid addresses get a red border, and Send is
  disabled until To/Cc/Bcc hold at least one valid address.
- **Keys** (context `Compose`, on the compose root):
  - ctrl-enter: Send
  - escape and ctrl-w: CloseCompose. If anything was edited, close asks "Discard this
    message?" through inline Discard/Keep buttons.
  - ctrl-q: Quit (with the same discard check for this window first).
  - tab / shift-tab move between fields (bound by `TextInput` and `TextArea` themselves).
- **Send** sets the state to Sending, which disables the controls, then calls `Store::send` and
  waits for `SendFinished`.
  - On success: the window closes and the main window posts the notice "Message sent".
  - On failure: an inline error. When `maybe_sent`, the message is shown and Send stays disabled
    until the user ticks "Send anyway".
- Forward includes the original attachments as `AttachmentSource::Gmail` chips, which can be
  removed (chips from M8; the Google worker resolves them from M12).

### 8.6 `TextArea` (`ui/text_area.rs`)

This is a new entity modelled on `text_input.rs`, built on `ui/text_layout.rs`.
- It implements `EntityInputHandler` with UTF-16 ranges and IME marked text.
- **Layout and scrolling:** it takes the flex height its parent gives it and scrolls itself: it
  keeps its own `scroll_offset`, paints inside `window.with_content_mask`, handles
  `ScrollWheelEvent`, and adjusts `scroll_offset` after every cursor move so the cursor stays
  visible. It does not depend on a parent `ScrollHandle`.
- It supports:
  - Cursor movement: left and right by grapheme, up and down by visual line with a goal column,
    home and end per visual line, ctrl-home and ctrl-end, ctrl-left and ctrl-right by word.
  - Selection with shift for every move, and select-all.
  - Editing: backspace and delete (ctrl variants by word).
  - Clipboard: paste **keeps newlines** (CRLF normalised to LF), cut and copy.
  - ctrl-z / ctrl-shift-z undo and redo, with edits coalesced by typing run.
  - Mouse click and drag to select.
- **Key bindings** (`text_area::bind_keys`, context `"TextArea"`). On Linux, `key_char` is `None`
  for control characters, so enter and tab never reach `replace_text_in_range` and must be
  bound explicitly:
  - `enter` → Newline, `shift-enter` → Newline
  - `tab` → FocusNext, `shift-tab` → FocusPrev
  - the movement, selection, editing, clipboard and undo keys above
  - `ctrl-enter` is **not** bound, so the `Compose` context receives it.
  - It never emits `Submit`.
- **Unit tests** cover pure buffer ops: a `TextBuffer` struct handling insert, delete, word
  boundaries, undo coalescing, and line/column mapping over wrap boundaries given as byte
  offsets (already converted from `WrapBoundary`).

---

## 9. Actions and key map (`ui/mod.rs`)

Actions are declared with `actions!(eloquawk, [...])`. Key contexts:
- **`Eloquawk`:** on the main window root, always active.
- **`Mail`:** permanently on the panes container (which exists only on `Screen::Mail`) that
  tracks `MainView.focus`. Mail bindings are registered with the predicate
  **`"Mail && !TextInput && !TextArea"`**. gpui evaluates `!X` against the whole context
  stack, so no Mail key fires while any text field nested anywhere inside is focused, and no
  field list has to be maintained by hand. `ReadOnlyText` is not excluded: Mail keys work while
  a message body is focused.
- **`ReadOnlyText`:** on each `BodyText` (ctrl-c Copy, ctrl-a SelectAll).
- **`LabelPicker`:** on the label picker popover. It binds `escape` (close), `up` / `down`
  (move highlight). Its filter is a `TextInput`, which binds `enter` to `Submit`; the picker
  handles that `Submit` as "toggle the highlighted label".
- **`Compose`:** on the compose root (section 8.5).
- **`Search`:** on the search input wrapper.

`bind_keys` calls `text_input::bind_keys`, `text_area::bind_keys` and `body_text::bind_keys`
first.

**Key names on Linux.** The Linux keystroke builder maps `#`, `!`, `?`, `:` to those characters
and clears `shift` for any single non-letter character, so `shift-3`, `shift-1` and `shift-/`
can never fire. Bind the characters only. Letter bindings with shift (`shift-i`, `shift-u`)
keep their shift and work.

| Key | Action | Context | Behaviour |
|---|---|---|---|
| `j` / `down` | NextThread | Mail | Cursor down (list follows). If the reader is open, opens the next thread. |
| `k` / `up` | PrevThread | Mail | Cursor up, with the same reader rule. |
| `n` / `p` | NextMessage / PrevMessage | Mail | Moves focus between message cards in the reader, expands the focused one and scrolls it into view. |
| `o` / `enter` | OpenThread | Mail | Opens the cursor thread. In the reader, toggles the focused message. |
| `u` / `escape` | BackToList | Mail | Closes the reader; focus returns to the list. Escape also clears the selection, then closes overlays. |
| `space` / `pagedown` | ScrollReaderDown | Mail | Scrolls the reader by one page. |
| `shift-space` / `pageup` | ScrollReaderUp | Mail | Scrolls the reader up by one page (`shift-space` keeps shift; see 15 item 3). |
| `x` | ToggleSelect | Mail | Toggles the cursor thread in the selection. |
| `e` | Archive | Mail | Removes INBOX. In Spam, does Not spam. |
| `#` | Trash | Mail | In Trash, does Restore. |
| `s` | ToggleStar | Mail | |
| `shift-i` | MarkRead | Mail | |
| `shift-u` | MarkUnread | Mail | |
| `!` | ReportSpam | Mail | Adds SPAM, removes INBOX. In Spam, does Not spam. |
| `l` | OpenLabelPicker | Mail | |
| `r` | Reply | Mail | Replies to the open (or cursor) thread's last message. |
| `a` | ReplyAll | Mail | |
| `f` | Forward | Mail | |
| `c` | Compose | Mail | |
| `z` | Undo | Mail | Undoes the last undoable notice. |
| `;` / `:` | ExpandAll / CollapseAll | Mail | |
| `/` | FocusSearch | Mail | |
| `g i` | GoInbox | Mail | Multi-keystroke binding. |
| `g s` | GoStarred | Mail | |
| `g t` | GoSent | Mail | |
| `g d` | GoDrafts | Mail | |
| `g a` | GoAllMail | Mail | |
| `g n` / `g p` | NextAccount / PrevAccount | Mail | Switches to that account's inbox. |
| `?` | ToggleHelp | Mail | |
| `ctrl-c` / `ctrl-a` | Copy / SelectAll | ReadOnlyText | |
| `escape` / `up` / `down` | ClosePicker / PickerUp / PickerDown | LabelPicker | |
| `enter` | Submit | Search | Searches the current account; the view becomes `Search(q)`. |
| `escape` | CancelSearch | Search | Clears the field and focuses the list. |
| `ctrl-enter` | Send | Compose | |
| `escape` / `ctrl-w` | CloseCompose | Compose | With the discard check. |
| `ctrl-q` | Quit | Compose | |
| `escape` | CloseSettings | Settings | Closes Settings (M9). |
| `ctrl-r` | Refresh | Eloquawk | |
| `ctrl-,` | OpenSettings | Eloquawk | |
| `ctrl-n` | Compose | Eloquawk | |
| `ctrl-w` | CloseWindow | Eloquawk | |
| `ctrl-q` | Quit | Eloquawk | |

**Bindings by milestone.** M4 binds the navigation keys (j, k, down, up, o, enter, u, escape)
so the demo script can drive the reader. M6 binds every other key in the table and the help
overlay lists the full table from M6 on. Keys whose feature lands later (`l`, `/` in M7; `c`,
`r`, `a`, `f`, `ctrl-n` in M8; `ctrl-,` in M9) are bound from M6 to handlers that post the
notice "Not available yet"; the owning milestone replaces the handler.

Focus rules:
- After opening a window, switching screens, submitting a search, or closing an overlay, focus
  `MainView.focus` explicitly with `window.focus(&h, cx)`.
- The help overlay (`ui/help.rs`) renders the table above from one `const SHORTCUTS` array.
  The README table is generated from the same data, by hand-copy, kept in sync.

Search behaviour:
- Submitting sets `view = Search(q)` for the current account. The header shows "Search results
  for "q" ✕". ✕ returns to the previous label view.
- The query is sent to Gmail **verbatim**. Gmail's own syntax is the contract.
- Demo mode matches the query with `mail/query.rs`.

`mail/query.rs` supports:
- Free terms (AND), case-insensitive
- `"phrases"`
- `-term` negation
- `OR` between two terms
- `from:`, `to:`, `cc:`, `subject:`
- `label:`, `in:inbox|sent|trash|spam|anywhere|starred`
- `is:unread|read|starred|important`
- `has:attachment`
- `newer_than:Nd|Nm|Ny` and `older_than:`
- `after:` and `before:`, as `YYYY/MM/DD`

Unknown operators are matched as free text. Parsing never fails.

---

## 10. Outgoing MIME (`mail/compose.rs`)

`pub fn build(out: &Outgoing, attachments: &[(OutgoingAttachment, Vec<u8>)], now: Timestamp, message_id: &str) -> Result<Vec<u8>, ComposeError>`
is pure: the clock and the id are injected.

The function builds the message with lettre's builder:
- `Message::builder().from(out.from)`, then `.to`, `.cc` and `.bcc` for each address
  - **`.keep_bcc()` is mandatory.** Gmail reads Bcc from the raw message and strips it on
    delivery.
- `.subject(out.subject)` (lettre RFC 2047-encodes non-ASCII)
- `.message_id(Some(format!("<{random_hex_16}@eloquawk.local>")))`, where the random part comes
  from getrandom and is injected in tests
- `.date(now)`
- `.in_reply_to(..)` and `.references(..)` (space-joined) when present
- `.user_agent("Eloquawk/<ver>")`

The body depends on whether there are attachments:
- **No attachments:** `SinglePart::plain(body)`, with lettre choosing QP or base64, charset
  UTF-8, and line endings normalised to CRLF by lettre.
- **With attachments:** `MultiPart::mixed()` holding the plain part, then one
  `Attachment::new(filename).body(bytes, ContentType)` per file. The MIME type falls back to
  `application/octet-stream`.

`message.formatted()` produces the bytes, which go into the `message/rfc822` part of the
multipart upload (6.2). `mail/b64` is used for decoding Gmail payloads only.

`mail/reply.rs` (pure, tested) works as follows:
- **Target message:** the last message in the thread that is not a draft.
- **Reply:**
  - To is `reply_to` if present, else `from`.
  - If the target is from me, To is the target's `to` instead (Gmail behaviour).
- **Reply-all:**
  - To is the reply's To plus the target's `to`.
  - Cc is the target's `cc`.
  - Remove my address and dedupe case-insensitively by email, keeping the first.
  - An address already in To never appears again in Cc.
- **Subject:** add `Re: ` unless the subject starts with `re:` (any case, after trimming). Forward
  uses `Fwd: ` unless it already starts with `fwd:` or `fw:`.
- **Threading:**
  - `thread_id = Some(thread.id)` for reply, reply-all and forward.
  - `in_reply_to = target.message_id`.
  - `references = target.references + [target.message_id]`, deduplicated, keeping the last 20.
- **Reply body:** `"\n\nOn {full_date}, {Name} <{email}> wrote:\n"` followed by each line of the
  target body prefixed with `"> "`, or `">"` for empty lines. The cursor starts at the top.
- **Forward body:** `"\n\n---------- Forwarded message ---------\nFrom: …\nDate: …\nSubject: …\nTo: …\n[Cc: …]\n\n" + body`.
  Forwards carry the target's attachments.

`mail/mailto.rs` parses `mailto:a@b,c@d?cc=..&bcc=..&subject=..&body=..` with
percent-decoding and the RFC 6068 rules, with a strict whitelist:
- Only `to`, `cc`, `bcc`, `subject` and `body` are honoured. Every other hfield (`attach`,
  `attachment`, `from`, `in-reply-to`, `references` and anything unknown) is ignored.
- The prefill is always `ComposeKind::New`, with `thread_id`, `in_reply_to` and `references`
  empty and no attachments (never `AttachmentSource::Local`).
- A mailto prefill is never sent automatically; it only opens the compose window.

---

## 11. Demo mode

- `--demo` or `ELOQUAWK_DEMO=1` sets `Mode::Demo`.
  - Prefs are `ephemeral`: loaded from disk for the theme, never saved.
  - No keyring or network. No instance claim unless `--instance` is given, in which case the
    demo lock and socket names are used (section 2.1).
  - Notifications still fire.
- `demo::start(prefs_snapshot) -> (SyncHandle, Receiver<SyncEvent>)` spawns `eloquawk-demo`.
- **Generated mailbox** (`demo_data.rs`), seeded and deterministic, relative to `Timestamp::now()`:
  - **Accounts:** `you@example.com` (about 120 threads) and `you@work.example` (about 40).
  - **Labels:** INBOX, STARRED, IMPORTANT, SENT, DRAFT, SPAM, TRASH, and the user labels
    "Receipts", "Travel", "Projects/Eloquawk" and "Projects/Website" with colours. **M3:** a
    "Projects" parent label is included too, because `labels_for` nests a label only under an
    ancestor that exists (without one it shows the full name at depth 0).
  - **Content:**
    - A variety of threads of 1 to 6 messages.
    - Some unread and some starred.
    - Some with attachments (generated bytes).
    - One long HTML newsletter with links (including one whose text differs from its href),
      converted through `mail/html.rs` so that path gets exercised.
    - One with a deep quote chain.
    - One with a 2FA-style code and a tracking number in the body (selection target).
    - One plain body over 200 KB (truncation path).
    - Non-ASCII names and subjects (e.g. "Zoë Ångström", "Café ☕ plans").
    - Threads in Spam and Trash.
    - Dates spread from minutes ago to 400 days ago.
- **Behaviour:** the demo holds `HashMap<ThreadKey, (ThreadSummary, ThreadDetail)>` and answers
  every command:
  - `Want`: filter by view (label membership, AllMail excludes SPAM and TRASH, `Search` uses
    `mail/query.rs`), sort by `last_date` descending, and page by `page_size`. `has_more` comes
    from that paging. Page 1 is `Replace`, later pages `Append`.
  - `OpenThread`: emits the detail.
  - `Modify`, `Trash` and `Untrash` (with `restore`): applied in memory, then
    `MutationFinished { None }` after a 150 ms sleep to mimic latency. Trash adds TRASH and
    removes INBOX.
    - A thread whose subject contains "[fail]" returns an error, so rollback can be seen.
  - `Send`: appends a SENT message to the thread, or creates a new thread, then
    `SendFinished { None }`. Gmail-sourced attachments are resolved from the demo's own bytes.
  - `SaveAttachment`: emits the notice "Demo mode doesn't save files" and writes nothing.
  - The account and client commands emit the notice "Demo mode: sign-in is disabled".
  - `Refresh`: re-emits the labels and the current view (`Refresh` mode).
- **New-mail simulation:** 20 s after start, one new unread INBOX thread arrives for
  `you@example.com` through `ThreadsUpdated`, `ThreadPage { mode: Refresh }` and `NewMail`.
  This makes the notification path verifiable.
- **`--demo-actions "<steps>"`** (demo only; `demo_script.rs` parses, `main` runs):
  - After the first `ThreadPage`, `main` runs the comma-separated steps on a `cx.spawn` timer,
    250 ms per step, against the focused window.
  - Step grammar:
    | Step | Effect | From |
    |---|---|---|
    | `wait:N` | sleep N ms | M4 |
    | `theme:light` / `theme:dark` | switch the palette | M4 |
    | `shot` | print `ELOQUAWK_READY <title of the focused window>` and flush stdout (**M4:** gpui's Wayland and X11 windows return an empty `window_title()`, so the app substitutes the title it gave the window; M8 must do the same for compose windows) | M4 |
    | `open:N` | set the cursor to row N and open it (fallback that needs no key bindings) | M4 |
    | `act:<Name>` | dispatch an action through the tested `demo_action(name) -> Option<Box<dyn Action>>` table with `window.dispatch_action` (for non-key steps) | M4 |
    | `type:<text>` | dispatch each character as a keystroke | M4 |
    | `expect:<cond>` | check `cursor=N`, `open=none|N`, `view=<label id>|search`, `focus=list|reader|search|compose`, `search=<text>` or `selection=<text>` (M6 adds `card=none|N`, `expanded=N`, `scroll=top|down`, `selected=N`, `rows=N`, `unread=yes|no` and `starred=yes|no` for the cursor thread, `notice=<text>|none`, `help=open|closed`, `account=<email>` and `syncs=N`; M8 adds the compose conditions; M9 adds `screen=mail|settings|auth`), and print `ELOQUAWK_EXPECT ok <cond>` or `ELOQUAWK_EXPECT fail <cond> (actual …)` | M4 |
    | `selectall:N` | focus card N's `BodyText` and select all | M5 |
    | `view:<label id>` | show that label of the current account (`ALL_MAIL` for All Mail); a fallback for views no key reaches, such as Spam and Trash | M6 |
    | `search:<query>` | set the search field and dispatch its Submit | M7 |
    | `attach:<path>` | add a local file to the focused compose window as if picked in the file chooser (the portal cannot be scripted), with the same 25 MB check | M8 |
    | `save:N` | press Save on the open thread's attachment N (0-based, counted across its messages in order); the button has no key | M12 |
    | anything else | one or more keystrokes separated by spaces, each parsed with `Keystroke::parse` and sent with `window.dispatch_keystroke(..)`, so bindings, context predicates and chords are exercised; `g i` sends `g` then `i`. `comma` stands for `,`. | M4 |
  - In `type:`, `search:` and `expect:` values, `\,` escapes a comma.
  - An unparseable or unknown step prints `ELOQUAWK_EXPECT fail step <step>` and is skipped.
  - Example: `"j,j,o,wait:800,s,#,theme:light,shot"`.

---

## 12. Testing strategy

### 12.1 Unit tests (inline, run in CI)

| Module | Cases |
|---|---|
| `mail/b64` | Padded and unpadded input, `-` and `_` characters, invalid input returns an error. |
| `mail/parse` | **Bodies and charsets:** multipart/alternative prefers plain; html-only goes through `html_to_text`; nested multipart/mixed with an attachment part (filename plus attachmentId); an attachment with inline `body.data` and no attachmentId; ISO-8859-1 and windows-1252 body charset; headers whose name case varies; a header still holding `=?utf-8?B?…?=`; missing payload. **Thread summaries:** `internalDate` parsing; summaries with unread and starred unions, participants and `has_attachments`; fixtures from `src/google/fixtures`. |
| `mail/html` | Drops script and style; br, p and li handling; numeric and named entities (`&#8217;`, `&hellip;`); `img` dropped; anchors render `text <href>` or the bare href, no footnotes or emphasis markers; input over 2 MiB is truncated and flagged; snippet entity decoding. |
| `mail/links` | http, https, mailto and `www.` detection; trailing punctuation excluded; `file:`, `javascript:` and custom schemes rejected; byte ranges correct around non-ASCII text. |
| `mail/address` | Quoted commas, display names, angle-only addresses, malformed input kept as text; the `participants_label` cases. |
| `mail/query` | Each operator, negation, OR, phrases, unknown operators, dates with an injected now. |
| `mail/reply` | Reply to someone else; reply to my own message; reply-all dedupe and self-removal; subject prefix idempotence; References chain; quoting; forward header block. |
| `mail/compose` | Round trip through the `mail-parser` dev-dependency: From, To, Cc **and Bcc present**, a UTF-8 subject decodes, In-Reply-To and References, a body with non-ASCII and long lines survives, multipart with an attachment (name, type, bytes), a deterministic Message-ID and Date. |
| `mail/mailto` | All whitelisted fields, percent-encoding, multiple recipients; `attach=`, `attachment=`, `from=` and `in-reply-to=` are ignored; kind is always New with no threading and no attachments. |
| `mail/files` | Sanitising each case of 7.5 step 4 (separators, NUL, controls, bidi overrides such as `invoice\u{202E}fdp.exe`, leading dots, `""`/`.`/`..`, 300-byte names keep the extension); unique naming; `user-dirs.dirs` with `$HOME/Downloads`, an absolute path, a relative path (rejected) and a missing file. |
| `google/api` | Error classification: 429, 403 user and daily limits (mixed case), 401, 404, 5xx including 504; `Retry-After` as seconds and as an HTTP-date; `Retry after <RFC 3339>` in the envelope (fixture); retry over 15 min becomes `QuotaExhausted`; body-limit overflow is `TooLarge`; body parsing of the error envelope. |
| `google/gmail` | Each DTO deserialises its fixture, including fields that are absent (empty history, no `threads`). Query building for repeated `historyTypes` and `includeSpamTrash` on TRASH/SPAM. |
| `google/oauth` | Copied tests, plus the built-in client with and without the compile-time overrides (an empty part means none), the granular-consent scope check, and token-endpoint error mapping (`invalid_grant`, `invalid_client`, 5xx). |
| `secrets` / `config` | Copied tests (MemoryStore, unknown fields tolerated, ephemeral never saves); `Prefs` round-trip through `save_to` / `load_from` in a temp dir. |
| `sync` | `Backoff` schedule, cap, HTTP-date hints and deferral (jobs parked, not dropped, and re-queued after `retry_at`); `collect_changes` with `fresh_inbox` filtering (categories excluded) and `messagesDeleted` folded into `changed`; `commit_round` + refetch failure keeps the ids pending for the next round; large-burst path still refetches fresh inbox ids; job-queue coalescing and lane order; the poll queues `InitialSync` (not `History`) after a failed `profile()`; send-error mapping to `maybe_sent` (network, 5xx true; 4xx false); auth network errors take the backoff path. |
| `store` | Built via `Store::for_tests(SyncHandle::from_sender(tx))` with ephemeral prefs. Named tests (all in M3): `apply_every_event`, `cursor_moves_and_requests_more`, `archive_removes_from_inbox_and_auto_advances`, `rollback_restores_index`, `pending_delta_reapplied_over_fresh_data`, `undo_applies_inverse`, `undo_trash_untrashes_and_restores_labels`, `spam_removes_from_label_views_and_all_mail`, `refresh_page_keeps_tail_and_cursor`, `append_dedupes`, `new_mail_only_when_pref_on`, `account_removal_prunes`, `selection_targets_bulk_mutation`. |
| `notify` | The seen set (first-sync suppression is the sync worker's and is covered by the `sync` tests, see M13 notes), grouping into one summary note when more than 3 arrive, body text; a subject or snippet containing `<img src=…>`, `<a href=…>` and `&` comes out escaped; control and bidi characters are stripped from summary and body. |
| `ui/theme` | Contrast ratios for both palettes. |
| `ui/text_layout` | Wrap-boundary to byte-offset conversion (pure part), selection range normalisation, word boundaries for double-click. |
| `ui/text_area` | `TextBuffer` operations. |
| `ui/format` | `list_date`: today shows the time, this week the weekday, this year "3 Oct", older "3 Oct 2025"; tested with an injected now and time zone. |
| `cli` | Flag parsing; `--demo-actions` without `--demo` is an error; `--instance` without `--demo` is accepted and ignored. |
| `demo_script` | Every step kind parses; `comma` and `\,` escapes; chords split into keystrokes; unknown steps reported. |
| `demo_script` (`demo_action`) | `demo_action` table coverage. **M4:** the table lives in `demo_script.rs` next to the step parser, so the worker module `demo.rs` stays free of UI actions. |

### 12.2 Headless end-to-end (`cargo test`, no window)

**`store::tests::demo_end_to_end`** (M3; the send step is added in M8):
1. Start `demo::start`.
2. Collect events for at most 2 s, applying each one to `Store::for_tests` connected to the
   demo's `SyncHandle`.
3. Assert:
   - auth is Demo
   - there are 2 accounts
   - INBOX shows a non-zero unread count
   - the first page has `page_size` rows
4. Then exercise the store:
   - `open_cursor()` yields a detail, and the thread is marked read.
   - Archive removes the thread from INBOX and `MutationFinished` arrives.
   - Undo puts it back.
   - A "[fail]" thread rolls back.
   - A search for `from:zoë` returns only matching rows.
   - (M8) Sending a reply appends to the thread.

This test uses the real channels and the real worker thread. It needs no GPUI window: the
`Store` is a plain struct, and only `cx.new` wraps it.

**Fake Gmail server** (`sync::tests`, modelled on antarctical `src/google/api.rs`
`patch_response_at` and its loopback `TcpListener` test). A small scripted HTTP/1.1 server on
`127.0.0.1:0` serves the hand-written fixtures, records every request (method, path, query,
body) and answers per a script. The real worker thread runs against it through `Endpoints`,
with a `MemoryStore` holding a client and one refresh token.
- **`fake_gmail_read_path`** (M10): token refresh, `profile`, `labels`, label counts,
  `threads` list, `thread_meta` chunks, `OpenThread` with `format=full`, `Want { more }` with a
  page token, a 429 with `Retry-After` causing deferral (not loss) of a `FetchMeta` chunk, and a
  failed `profile` followed by a poll that queues `InitialSync`.
- **`fake_gmail_write_path`** (M11): `Modify` hits `/threads/{id}/modify` per thread with the
  expected JSON body; star hits `/messages/{last}/modify`; `Trash` / `Untrash { restore }`;
  history with one `messageAdded` emits `NewMail` only after the first sync; a `thread_meta`
  500 during refetch leaves the id pending and the next round refetches it; `messagesDeleted`
  plus a 404 refetch emits `ThreadsGone`; history 404 emits `AccountReset` and re-runs initial
  sync; a 401 triggers one refresh and a retry; `Send` posts `multipart/related` to the upload
  path, whose JSON part carries `threadId` and whose rfc822 part parses with `mail-parser`; a
  503 on send yields `maybe_sent: true`.

### 12.3 Visual verification with niri (milestones that touch the UI)

`scripts/screenshot.sh <name> [demo-actions] [theme]` is a bash script that does the following:
1. Runs `cargo build`.
2. Starts `target/debug/eloquawk --demo --theme ${3:-dark} --demo-actions "${2:-wait:1500},shot"`
   in the background and waits up to 20 s for `ELOQUAWK_READY <title>`.
3. Finds the window by the printed title:
   `niri msg -j windows | jq -r --arg t "$title" '.[] | select(.app_id=="korora.tech.Eloquawk" and .title==$t) | .id' | head -1`.
   This picks the compose window, not the main one, for `compose` and `reply` shots.
4. Captures it with
   `niri msg action screenshot-window --id "$id" --path "$PWD/target/shots/$1.png" --write-to-disk true`.
   Check `niri msg action screenshot-window --help` and adjust the flags if they differ.
5. Prints every `ELOQUAWK_EXPECT` line, exits non-zero if any is `fail`, and kills the process.
6. Leaves the clipboard as it found it: the capture (and any scripted ctrl-c) replaces it, so
   the script saves it with `wl-paste` first and restores it with `wl-copy` on exit, or clears
   it when it was empty. wl-copy serves one MIME type, so one representation is kept (plain
   text if offered, else PNG, else the first type).

The verifying agent then opens the PNG with the Read tool and checks the milestone's visual
acceptance criteria. Shots in `target/shots/` are never committed. The one committed exception
is `docs/screenshots/*.webp`, produced from demo-mode shots with `cwebp` (libwebp, in the dev
shell) in M15.

This opens a real window on the user's desktop. Keep runs short and always kill the process.
If niri is unavailable, say so in the report; do not claim visual verification.

Minimum shot set per UI milestone: `inbox-dark`, `inbox-light`, `reader` (`j,j,o`), and the
milestone's own feature, e.g. `selection` (`j,o,selectall:0`), `compose` (`c`), `help` (`?`),
`search`, `labels` (`l`) or `settings` (`ctrl-comma`).

**Verifying without the user's keyring or desktop state:**
- Non-demo launches during development use a debug build with `ELOQUAWK_SECRETS=memory`
  (section 6.4), so the Secret Service is never touched.
- Single instance is verified with `--demo --instance`, which uses the demo lock and socket
  names and never collides with a real instance.
- Notifications are counted with
  `timeout 30 dbus-monitor --session "interface='org.freedesktop.Notifications',member='Notify'"`
  running alongside the app; count the `Notify` method calls.

### 12.4 Live Google check (manual, not in CI)

This is only possible when the user has supplied a client with the Gmail API enabled and test
users added:
- sign in
- the inbox lists
- open a thread
- archive, then undo
- star
- send a message to self
- a reply (with an attachment) threads correctly in Gmail web
- an attachment saves
- a new mail notification appears within one refresh interval

Agents must not claim this was done unless it was actually performed.

---

## 13. Milestone plan

Every milestone must pass the gate in section 1 on its own, including the dead-code policy.
The commit messages below are the milestone's final or main commit; finer commits within a
milestone are welcome. **No trailers.** Push to main after each commit.

| # | Title | Commit |
|---|---|---|
| M1 | Scaffold | `chore: scaffold eloquawk gpui application` |
| M2 | Mail domain and parsing | `feat(mail): add domain model and gmail payload parsing` |
| M3 | Protocol, store and demo worker | `feat(store): add worker protocol, optimistic store and demo mailbox` |
| M4 | Three-pane UI and demo scripting | `feat(ui): add three-pane mail view with light and dark themes` |
| M5 | Selectable message text and links | `feat(reader): add selectable message text and safe links` |
| M6 | Keyboard shortcuts and action wiring | `feat(ui): add gmail keyboard shortcuts and undoable actions` |
| M7 | Search and labels | `feat(search): add gmail query search and label picker` |
| M8 | Compose, reply and forward | `feat(compose): add composer with reply, reply-all and forward` |
| M9 | Google sign-in, keyring and settings | `feat(google): add oauth sign-in, keyring storage and settings` |
| M10 | Gmail read path | `feat(sync): list and open gmail threads with initial sync` |
| M11 | Gmail incremental sync, mutations and send | `feat(sync): add history refresh, mutations and sending` |
| M12 | Attachments | `feat(attachments): save attachments and forward them` |
| M13 | Notifications and single instance | `feat: notify about new mail and hand off to a running instance` |
| M14 | Desktop integration and Nix module | `feat(nix): add desktop entry, icon and home manager module` |
| M15 | Documentation and polish | `docs: add readme, gmail setup guide and screenshots` |

### M1: Scaffold
- **Files:**
  - `Cargo.toml` (final dependency set from 2.2, so the lock seeds early) and `Cargo.lock`,
    seeded from antarctical
  - `rust-toolchain.toml`, `.gitignore` (antarctical's, plus `/target/shots` implied)
  - `LICENSE` (Apache-2.0 copied from antarctical)
  - `flake.nix` + `flake.lock`, with the package, dev shell and formatter. Adapt antarctical's
    flake as follows: the package fileset is `./Cargo.toml ./Cargo.lock ./LICENSE ./src` only
    (no `./data`); no `postInstall` desktop or icon lines; no `dux-plugin`; no `homeModules`
    or hm checks. `cargoLock` keeps `allowBuiltinFetchGit = true`, so git dependencies need no
    `outputHashes`. The dev shell adds `libwebp` (cwebp) and `jq`.
  - `.github/workflows/ci.yml`, `.github/dependabot.yml`, `.github/labeler.yml`,
    `.github/workflows/pr-labeler.yml`
  - `src/main.rs`: a minimal window, 1280×820, title "Eloquawk", app_id `korora.tech.Eloquawk`,
    with ctrl-q to quit, quitting when the last window closes
  - `src/branding.rs`, with `APP_ID` only
  - `docs/SPEC.md`, this file, committed here
- **Acceptance:**
  - The gate passes.
  - `nix build -L` succeeds.
  - `cargo tree -i tokio` is empty.
  - The window opens and appears in `niri msg -j windows` with the right app_id.

### M2: Mail domain and parsing
- **Files:** `model.rs`, `mail/{mod,b64,parse,html,links,address,query}.rs`,
  `google/{mod,gmail}.rs` (DTOs only), `google/fixtures/*.json`
- **Acceptance:** all section 12.1 tests for these modules pass. Unused items carry per-item
  `#[allow(dead_code)] // consumed in Mx` (section 1).

### M3: Protocol, store and demo worker
- **Files:** `protocol.rs`, `config.rs`, `store.rs` (the **whole** Store of 8.1: queries,
  navigation, page merging, selection, mutate, rollback, undo, auto-advance, `new_mail`),
  `demo.rs`, `demo_data.rs`, `cli.rs` (`--demo`, `-h`, `-V`)
- **Acceptance:**
  - All the named `store` tests in 12.1 pass.
  - `demo_end_to_end` (12.2) passes, all steps except the send step (M8).
  - `config` tests pass, including the `Prefs` round-trip.
  - `main` accepts `--demo` and logs the event count.
  - `Command` variants first constructed later (account and client commands, `Send`,
    `SaveAttachment`, `SignInFinished`) and `SyncEvent` variants the demo never emits carry
    per-item allowances naming their milestone.

### M4: Three-pane UI and demo scripting
- **Files:** `ui/{mod,theme,widgets,format,windows,main_view,sidebar,thread_list,reader,auth}.rs`,
  `ui/text_input.rs` (copied), `demo_script.rs`, `scripts/screenshot.sh`, `main.rs` wiring
  (event pump, `--theme`, `--demo-actions` runner), `cli.rs` (`--theme`, `--demo-actions`)
- **Scope:**
  - Reader as a cached `ReaderView` entity with its own scroll handle and offset reset; bodies
    rendered as plain wrapped text for now (selectable in M5); attachment rows with a disabled
    Save button; empty header icon slot.
  - Thread list with `UniformListScrollHandle` following the cursor and the footer as a
    sibling.
  - Navigation actions NextThread, PrevThread, OpenThread, BackToList, bound to j, k, down, up,
    o, enter, u and escape with the `"Mail && !TextInput && !TextArea"` predicate.
  - The demo script steps marked M4 in section 11.
- **Acceptance:**
  - `--demo` shows the accounts, labels with counts, the thread list (uniform_list, fixed rows)
    and the reader.
  - Clicking a label, a thread or a star works; messages collapse and expand; quoted text folds.
  - The light and dark palettes both pass the contrast tests and both look right in
    `inbox-dark`, `inbox-light` and `reader` (`j,j,o`) shots, all producible with M4's code.
  - `screenshot.sh x "j,j,o,expect:cursor=2,expect:open=2,u,expect:open=none"` reports only
    `ok` lines.

- **M4 notes:**
  - The reader already cuts bodies over 200 KB and shows the "Converted from HTML" and
    "Message truncated" captions (plain text, not yet a link); M5 keeps the cut when bodies
    become `BodyText`, and adds the Copy button.
  - Archive, Trash/Restore, Not spam, Mark unread and Star in the action bar, the row star
    and the row checkbox work by click from M4 (they only call the M3 Store). Labels…, Reply,
    Reply all, Forward, Compose and Settings render disabled until their milestones.
  - `Screen` has `Mail` and `Auth` only; `Settings` is added with the settings screen in M9.
  - `BackToList` (u, escape) closes the reader, or clears the selection when it is already
    closed. M6 may split escape's overlay handling out.
  - `expect:selection=` always reports an empty selection until M5.
  - The active account's section is always expanded, whatever `collapsed_accounts` holds, and
    its header does not collapse it.
  - The reader gives each message its default expansion (unread or last) the first time it
    sees it, so a message that arrives in the open thread later opens too while the user's
    toggles on the others are kept.
  - Pointer acceptance (clicking a label, thread, star, card header, quote toggle) is
    verified by reading the handlers only: the dev environment has no pointer injection tool
    (no `ydotool`), and the demo script drives the keyboard and actions only.

### M5: Selectable message text and links
- **Files:** `ui/text_layout.rs`, `ui/body_text.rs`, `ui/reader.rs` (bodies as `BodyText`,
  Copy button, link clicks, large-body truncation), `mail/mailto.rs` (whitelist parser, used
  for mailto link clicks), demo script `selectall:`
- **Acceptance:**
  - The `text_layout`, `links` and `mailto` tests pass.
  - Mouse drag selects text within a body; ctrl-a then ctrl-c in a focused body puts the body
    on the clipboard. Take the shot with `screenshot.sh selection "j,o,selectall:0,shot"`;
    check the clipboard separately (the capture overwrites it, see the notes) by launching the
    binary in the dev shell with `--demo-actions "j,o,selectall:0,ctrl-c,wait:1000"` and, if
    `wl-paste` is available, reading `wl-paste -n` before it exits; it matches the start of the
    demo body. Otherwise record that the clipboard was not checked.
  - Dropping a focused body (thread change, card collapse, quote fold) hands the focus to the
    reader, so Mail keys keep working:
    `"open:0,selectall:0,j,j,expect:open=2,expect:focus=reader"` reports only `ok`.
  - Escape in a body with a selection clears it; with none it closes the thread:
    `"open:0,selectall:0,escape,expect:selection=,expect:open=0,escape,expect:open=none"`.
  - The "Copy" button copies the whole body.
  - Links in the demo newsletter are underlined, and the one whose text differs from its href
    shows the href. Clicking is verified by inspection of the handler (`open::that` is not
    exercised in automated runs); `file:` and `javascript:` text is not linked.
  - Shots: `reader`, `selection`, `newsletter`.

- **M5 notes:**
  - Until the composer lands (M8), a mailto link click runs the whitelisted parser and posts
    the notice "Writing mail is not available yet"; M8 opens the prefill instead.
    `mailto::parse(url, account, from) -> Option<Outgoing>` takes the sending account, since
    `Outgoing` needs one.
  - `expect:selection=<text>` compares exactly. The reader trims trailing whitespace off the
    shown body, so a whole-body selection has no trailing newline.
  - `selectall:N` expands card N if it is collapsed before selecting.
  - The card's Copy button copies what is visible: the body, plus the quote when unfolded. The
    "Open in Gmail" part of the HTML and truncation captions is now a link.
  - A selection spans one body: pressing in a body clears the others. Collapsing a card drops
    its body entities, and with them any selection.
  - `SelectableText` registers a window-wide mouse move listener, so a drag that leaves the
    body keeps extending the selection.
  - A body takes focus on mouse down and ctrl-a. The reader never drops a body entity while
    it holds the focus without first focusing the reader itself (`ReaderView::drop_body`);
    otherwise keys would dispatch from the root, outside the Mail context.
  - Escape is also bound in `ReadOnlyText` (`ClearSelection`); with no selection the handler
    propagates and the Mail `BackToList` binding runs.
  - `niri msg action screenshot-window` also puts the PNG on the clipboard, so the ctrl-c
    check with `wl-paste -n` must run without a `screenshot.sh` capture (launch the binary
    in the dev shell with the same `--demo-actions` and read the clipboard before killing it).

### M6: Keyboard shortcuts and action wiring
- **Files:** `ui/mod.rs` (full key map with the Mail predicate), `ui/main_view.rs` (action
  handlers, focus, toast with Undo, reader scroll keys, n/p card focus via
  `scroll_to_item(i + 1)`), `ui/help.rs`, reader action-bar Restore / Not spam
- **Acceptance** (each verified with keystroke steps through `dispatch_keystroke` and
  `expect:` lines):
  - Keys owned here work: j, k, n, p, o/enter, u/escape, space, shift-space, pageup, pagedown,
    x, e, #, s, shift-i, shift-u, !, z, ;, :, g i, g s, g t, g d, g a, g n, g p, ?, ctrl-r.
  - `l`, `/` (M7), `c`, `r`, `a`, `f`, `ctrl-n` (M8) and `ctrl-,` (M9) are bound and post "Not
    available yet"; the help overlay lists the full section 9 table.
  - Archive auto-advances; Undo (toast and `z`) restores; a "[fail]" thread rolls back with a
    notice; in the Trash view `#` restores; in the Spam view `e` does Not spam.
  - `x` selection applies bulk actions.
  - Shots: `reader` via `j,j,o`, `help` via `?`, `archived` via `j,j,o,e,shot`.

- **M6 notes:**
  - The `archived` shot uses `j,j,o,e`: row 1 of the demo inbox is the "[fail]" thread, so
    `j,o,e` shows the rollback notice instead of an archive.
  - Escape (and `u`, the same action) closes the help overlay first when it shows, then the
    reader, then clears the selection. The overlay also closes on `?` and on a click outside
    the card; it leaves the focus on the list. Escape with the reader open does not also clear
    the selection (section 9 reads otherwise): `u` and escape share `BackToList`, and each
    press undoes one layer.
  - While the overlay shows, every other Mail action does nothing (`unless_help` in
    `ui/main_view.rs`), so `e`, `#` or `j` cannot change threads hidden behind it. The scrim
    `.occlude()`s, so a click on it closes the overlay without reaching the row, star or
    sidebar underneath, and hover and scroll do not reach the panes either.
  - `n` / `p` with no card focused start at the first / last card. The focused card has an
    accent border. `o` / enter toggles the focused card only while the open thread is the
    cursor thread; otherwise it opens the cursor thread.
  - `s` stars the targets unless every one is already starred, then it unstars. The reader's
    Archive / Not spam and Trash / Restore buttons use the same view-aware store helpers
    (`archive_kind`, `trash_kind`) as the keys, but every reader action-bar button acts on the
    open thread only (`mutate_threads`), not on an `x` selection: its Star / Unstar label
    describes that thread, so Star toggles that thread's own star.
  - `g n` / `g p` wrap round the account list.
  - `Store.syncs` counts `Synced` events so `expect:syncs=` can show that ctrl-r refreshed.
  - The "Not available yet" handlers live on the window root, so `c` (Mail) and ctrl-n
    (Eloquawk) reach the same one.
  - A rollback puts the thread back at its index but leaves the cursor and open thread where
    the optimistic change moved them (the store's M3 behaviour; not changed here).
  - The help table is `ui/help.rs::SHORTCUTS`; a test checks that its Mail and Eloquawk rows
    match `ui::bindings()` exactly and that every Mail binding carries the Mail predicate.

### M7: Search and labels
- **Files:** header search wiring, the `Search` context, `ui/label_picker.rs` (`LabelPicker`
  context), demo script `search:`
- **Acceptance:**
  - `/` focuses search. Enter shows results and ✕ restores the view.
  - Typing in search never triggers mail shortcuts:
    `"/,j,expect:focus=search,expect:search=j,expect:cursor=0"` reports only `ok`.
  - `l` opens the picker with a filter; up/down move, enter toggles, escape closes. Toggling
    applies a `Modify` optimistically and the sidebar counts update. Typing `e` in the filter
    does not archive.
  - Shots: `search` via `search:from:zoë`, `labels` via `j,l`.

- **M7 notes:**
  - The search field sits in the header, outside the `Mail` container, inside a `Search`
    wrapper that handles the field's `Submit` (enter) and `CancelSearch` (escape). Submitting
    calls `Store::search`, which trims the query, remembers the view it replaces and shows
    `Search(q)` for the current account; a query with no terms (`Query::is_empty`) leaves the
    search instead. Escape only clears the field and focuses the list; the results stay.
  - The header shows "Search results for “q” ✕" next to the field. ✕ dispatches the unbound
    action `CloseSearch` (`Store::end_search`), which returns to the replaced view, or the
    account's inbox when that view is unknown or belonged to another account. `act:CloseSearch`
    reaches it from scripts. The field is refilled from the shown search and cleared once the
    view leaves search.
  - The label picker is a `LabelPicker` entity drawn over a scrim (click outside closes) below
    the reader's action bar, outside the `Mail` container, so its filter (`TextInput`) never
    triggers mail keys. It lists every user label (hidden ones too; hiding only affects the
    sidebar), sorted by full name and filtered case-insensitively by substring. Each row shows
    ✓ when every target has the label, – when some do. Enter or a click adds the label to all
    targets unless all have it, in which case it removes it; the picker stays open for more
    toggles. Up and down keep the highlighted row scrolled into view. The card hangs from the
    reader pane's last laid-out bounds (so it follows the banner), and leaving the Mail screen
    drops it. `Store::set_label` makes it an undoable `Modify` with the toast
    "Added “Travel” to 1 conversation" / "Removed “Travel” from …".
  - `l` targets the selection, else the open thread, else the cursor row; the reader's
    "Labels…" button targets the open thread only (as its other buttons do). With no target,
    `l` posts "No conversation to label".
  - `expect:` gains `focus=picker`, `picker=open|closed`, `filter=<text>`,
    `label=<name>:yes|no` (the cursor thread) and `badge=<label name>:<N|none>` (the current
    account's sidebar count).

### M8: Compose, reply and forward
- **Files:** `ui/text_area.rs` (on `text_layout.rs`), `ui/compose.rs`, `ui/windows.rs`
  (compose window, initial focus), `mail/{reply,compose}.rs`, the demo `Send` (including
  Gmail-sourced chips from demo bytes), `Store::send`, the mailto link click now opening
  compose
- **Acceptance:**
  - `c`, `r`, `a`, `f` and `ctrl-n` open a prefilled compose window, with focus on To (new,
    forward) or the body (reply).
  - The body supports multi-line editing (enter inserts a newline), tab and shift-tab move
    fields, paste with newlines, undo and IME; the body scrolls itself and keeps the cursor
    visible.
  - ctrl-enter sends in demo and the reply appears in the thread. Escape or ctrl-w with edits
    asks before discarding. ctrl-q works from the compose window.
  - Attaching more than 25 MB in total is rejected inline.
  - The MIME round-trip tests pass, with Bcc kept. The `demo_end_to_end` send step passes.
  - Shots: `compose` (`c`), `reply` (`j,o,r`), both selected by window title.

- **M8 notes:**
  - `ui/text_layout.rs` now exposes the shaping it does for `BodyText` (`set_signature`,
    `shape`, `set_bounds`, `paint`, `position_for_index`, `wrap_offsets`), and its cache key is
    the text runs rather than the link ranges and colours. `TextArea` uses it from its own
    element, which takes the parent's height, shapes at its width, keeps a scroll offset and
    paints inside a content mask. Home and end are per visual line (`line_starts` combines
    newlines with the soft-wrap offsets); up and down aim for a pixel goal column through the
    layout, falling back to a char column before the first paint.
  - `TextInput` drops a bare line break that reaches it as text and turns embedded ones into
    spaces (a one-line field, and gpui's `shape_line` panics on a newline). In the compose
    window, enter in a one-line field (the field's `Submit`) moves to the next field, and the
    address fields get a red border (`TextInput::set_invalid`) when any entry fails lettre's
    address parser, which is what `mail/compose.rs::build` needs.
  - Actions: `Send`, `CloseCompose` and the unbound `DiscardCompose` (the inline "Discard"
    answer); ctrl-q in the `Compose` context is the same `Quit` action, which the compose root
    handles with the discard check before the app-wide handler can quit. A second escape while
    "Discard this message?" shows keeps the message. A window whose send is in flight closes
    without asking (the message is already with the worker).
  - `r`, `a` and `f` answer the open thread, else the cursor row; when its detail is not loaded
    the thread opens in the reader and the compose window follows once the detail arrives
    ("Couldn't load the conversation" if it fails). The reader's Reply, Reply all and Forward
    buttons dispatch the same actions. Switching From to another account on a reply drops the
    thread id (thread ids are per account) but keeps In-Reply-To and References.
  - After a successful send into a thread whose detail is cached, the store asks for that
    detail again (the old one stays on screen until the new one arrives), so the reply shows
    in the reader.
  - The demo `Send` builds the real MIME with `mail/compose.rs` (so a bad address or an
    oversized attachment fails as it would in Google mode), resolving forwarded attachments
    from generated bytes (`demo_data::attachment_bytes`) and reading local ones with the 25 MB
    budget; this is the demo's only file access. The new message keeps the generated
    Message-ID.
  - `expect:` gains `compose=to|cc|bcc|subject|body|none|open` (the focused compose window's
    field), `compose-error=<text>|none`, `attached=N`, `discard=asking|no` (all answered by the
    focused compose window), `composers=N` (open compose windows) and `messages=N` (the open
    thread's message count); `focus=compose` holds when a compose window is focused. `shot`
    prints a compose window's own title, since gpui reports empty titles.
  - **Review fixes.** A window closed while its send is in flight hands the op to the store
    (`Store::abandon_send`). If that send fails, the main window shows the notice "Couldn't
    send the message" ("The message may not have been sent" when `maybe_sent`). A window that
    has seen its outcome calls `forget_send`, so `sends` does not grow. The window manager's
    close button runs the same discard check through `on_window_should_close`. ctrl-q from
    any window goes through `compose::request_quit`: each edited compose window comes forward
    in turn and asks, and the app quits once none is left. A pending reply or forward is
    dropped if the open thread changes before its detail loads. `TextInput::set_text`,
    `mailto::parse` and `reply_subject`/`forward_subject` turn CR and LF into spaces
    (`mail::one_line`). The body's undo history keeps at most 200 steps (`UNDO_LIMIT`).
  - **Script target without focus.** When no window has the compositor's focus (an idle or
    locked niri session gives none to new windows), `--demo-actions` steps go to the newest
    compose window, and to the main window when there is none. Before, they always went to the
    main window. In that state, `focus=compose` means that the step target is a compose window.
    niri also captures nothing and delivers no close requests then, so shots need an active
    session.

### M9: Google sign-in, keyring and settings
- **Files:** `google/{oauth,api}.rs`, `secrets.rs` (with `ELOQUAWK_SECRETS=memory`), the
  `sync.rs` skeleton (auth, client, accounts, tokens, problems, `Endpoints`; no mail sync yet
  beyond `profile`), `ui/settings.rs`, the `NeedsClient` card, and `main.rs` choosing the
  worker
- **Acceptance:**
  - With a debug build, `ELOQUAWK_SECRETS=memory` and no `--demo`, the "Set up a Google
    client" card appears when the build has no built-in client (shot `needs-client`; build
    with `ELOQUAWK_GOOGLE_CLIENT_ID=""` once the built-in client is filled in), and the "Sign
    in to Google" card when it has one; the user's keyring is never touched.
  - Pasting a client saves it through `MemoryStore` in the tests.
  - The oauth (including the scope check), secrets and api tests pass.
  - Settings opens with ctrl-, (shot `settings`); preference persistence is covered by the
    `Prefs` round-trip test and by toggle handlers calling `save` only when not ephemeral.
  - No calendar strings remain (`rg -i calendar src` is empty). **M9 note:** the one hit is
    the `.ics` → `text/calendar` attachment MIME type in `mail/compose.rs`, which is mail, not
    antarctical residue, and stays.
- **M9 notes:**
  - The built-in client (section 1) holds the user's Desktop client id. Its secret could not
    be written by the implementing agent (its tool permissions refused to read the client
    JSON's secret), so `BUILTIN_CLIENT_SECRET` in `google/oauth.rs` is empty for now and a
    default build behaves as having no built-in client (the needs-client card) until the
    secret is pasted into that constant. The follow-up fix pass hit the same refusal (writing
    the secret into the source was blocked as credential leakage), so this stays open for the
    user. The sign-in card was checked visually with a throwaway
    `ELOQUAWK_GOOGLE_CLIENT_SECRET` override at build time instead.
  - Refresh failures other than invalid grant, invalid client and network (say a 400
    `invalid_request`) go into the banner for that account. An unreadable stored client is
    removed, together with its tokens, and the banner points to Settings instead of the
    keyring. Neither keyring parse errors nor token-endpoint parse errors quote the stored or
    received value, since that could be a token. Saving a different client writes the
    keyring first and signs out the old accounts only after that write succeeds.
  - `Endpoints` gains `revoke`, so a sign-out in a test never reaches Google.
  - `AuthError::Network` holds an `api::NetKind`, like `ApiError::Network`, and a 5xx from
    the token endpoint is `AuthError::Server(status)`; the worker treats both as the
    `"network"` problem (backoff arrives with M10).
  - Granular consent without the Gmail scope: the account cannot be named (`profile` needs
    the scope), so the new grant is revoked only when no account is signed in at all;
    otherwise it is left alone, since revoking could sign out an existing account. Nothing
    is saved either way.
  - Until M10 and M11 the Google worker answers `Want` with an empty first page,
    `OpenThread` with `ThreadFailed`, mutations with a `MutationFinished` error, `Send` with
    a `maybe_sent: false` error and `SaveAttachment` with a notice, all saying the feature
    "isn't available in this build yet". It checks each account with `profile` after the
    keyring loads and on Refresh. The refresh interval pref is saved but only reaches the
    worker with M10's poll.
  - `ELOQUAWK_SECRETS_SEED` is a JSON file `{"client": {"client_id", "client_secret"},
    "tokens": {"<email>": "<refresh token>"}}`, both keys optional.
  - The loopback fake server lives in `google/fake.rs` (`#[cfg(test)]`), shared by the api,
    oauth and sync tests; M10 extends it for `fake_gmail_read_path`.
  - Settings is `Screen::Settings`, opened with ctrl-, or the header button and closed with
    escape (`CloseSettings`, context `Settings`) or ✕. Picking a theme there clears a
    `--theme` override for the session.

### M10: Gmail read path
- **Files:** `sync.rs` (job queue and lanes, poll rule, backoff with deferral, initial sync,
  `ListView`, `FetchMeta` chunks, `missing` re-fetch, chunked label counts, `OpenThread`),
  `google/gmail.rs` read endpoints, the fake server test harness
- **Acceptance:**
  - The `Backoff`, queue, poll and api tests pass.
  - `fake_gmail_read_path` passes.
  - The live read steps of 12.4 (sign in, inbox lists, open a thread) are done if a client is
    available; otherwise marked not performed.

- **M10 notes:**
  - `Command::Tune { refresh_secs, page_size }` is new (section 5, 7.2 step 5);
    `sync::start_google` takes `&Prefs`. The durations (poll, backoff base 5 s, cap 300 s,
    jitter below 1 s) live in a private `Tuning` so tests shrink them.
  - The queue is two `VecDeque`s (interactive, background) rather than one with two lanes.
    `FetchMeta` chunks queued by a listing go to the *front* of the background lane, so the
    rows someone is looking at fill in before label counts.
  - A retryable failure (rate limit, network, 5xx, decode, quota) of a `FetchMeta`, `Labels`
    or `ListView` job parks the unfinished part of that job in the account's `deferred`, so a
    429 defers the chunk rather than losing it. `InitialSync` and `History` are not parked:
    the poll queues them again (7.3). A non-retryable `ListView` failure (a search Gmail
    refuses) answers with an empty page so the list stops loading; the reason is in the
    banner. The first success after a backoff clears the account's problem as well as
    `"network"`, so a rate limit that passed does not stay in the banner.
  - `AccountState` keeps the last labels and counts, so the full `Labels` list (which has no
    counts) is emitted with the previous counts instead of zeros. Counts are fetched for the
    system labels of section 4.1's table and every user label that is not hidden.
  - A thread that 404s during `FetchMeta` is emitted as `ThreadsGone`. `OpenThread` also
    refreshes the thread's summary (and so `has_attachments`) and emits `ThreadsUpdated`
    when it changed.
  - Until M11, the `History` job is an interim round: `profile`, and only if its history id
    moved, a `Refresh` re-list of page 1 of INBOX and the shown view (changed threads are
    refetched by their list `historyId`) plus INBOX, SPAM and DRAFT counts. It never emits
    `NewMail`. `Refresh` queues `InitialSync` for every account after resetting backoff; a
    repeat initial sync merges with `PageMode::Refresh` instead of `Replace`.
  - The fake server gains `Reply::sticky()` for requests whose count depends on timing (the
    poll's `profile`); it stops once every one-off reply is used.
  - Live read steps of 12.4 were not performed by the implementing agent; they are left for
    the user.
  - Review fixes: a long rate limit keeps its hint as `QuotaExhausted.retry_after`, and the
    worker also treats any `RateLimited` over 15 minutes as a quota problem (message and
    timing). While an account already waits, a retryable failure (only interactive jobs run
    then) does not raise `attempts`; it only lengthens `retry_at` to a longer hint, and a
    quota wait is never shortened. A thread whose metadata is refused without a retry (403,
    `TooLarge`) moves from `missing` to `refused` with its listed history id; neither the
    poll nor a listing fetches it again until a listing shows a new history id. The poll
    queues only skeleton ids that no queued or parked `FetchMeta` chunk holds, and a listing
    takes its ids out of queued chunks before queuing them at the front (ids in parked
    chunks wait for the backoff). Next-page tokens of searches are dropped when the shown
    view moves away from them. Not changed: a parked `ListView` leaves the list loading until
    the backoff passes (the worker keeps no per-view id list to answer with), and `known`
    still grows over a session (eviction is revisited in M11).

### M11: Gmail incremental sync, mutations and send
- **Files:** `sync.rs` (`History`, `collect_changes`, pending sets and `Refetch`, `NewMail`,
  `AccountReset`, `Mutate` with per-thread modify, star via message modify, untrash restore,
  `Send` via multipart upload with timeouts and `maybe_sent` mapping; Gmail-sourced
  attachments return the "isn't available yet" error), `google/gmail.rs` write endpoints
- **Acceptance:**
  - The `collect_changes`, pending-cursor, send-error and auth-backoff tests pass.
  - `fake_gmail_write_path` passes.
  - The remaining live steps of 12.4 (archive and undo, star, send to self, reply threads)
    are done if a client is available; otherwise marked not performed.

- **M11 notes:**
  - `gmail::history` returns `ApiError::NotFound` for an expired cursor; the worker treats
    that as `HistoryExpired` (no separate error variant). More than 100 history pages in one
    round is treated the same way (reset and initial sync) rather than followed.
  - `collect_changes` also folds each record's `messages` list into `changed`, and collects
    the label ids of `labelsAdded` / `labelsRemoved` so their counts are refreshed with the
    labels the refetched threads carried before and after (step 6). Counts are only fetched
    for labels the sidebar shows (a `LabelCounts` for any other label would add it to the
    sidebar).
  - The worker keeps each thread's star target (the last non-draft message id) instead of
    every message id.
  - A refetch refused for good (403, `TooLarge`) drops the id from `pending_changed`, so one
    unreadable thread cannot hold a round open forever. Retryable failures stop the chunk and
    leave the ids pending, as specified.
  - Large bursts pick the "newest" fresh ids by thread id (longest, then greatest): Gmail
    thread ids grow with time. Fresh ids beyond the 20 kept are dropped without a report.
  - Step 7's `NewMail` is emitted even when step 5's re-list fails (the candidates are
    cleared at that point, so they would otherwise be lost); `Synced` is emitted only when
    the re-list worked.
  - A repeat initial sync after one has completed (Refresh)
    keeps an existing history cursor instead of moving it to `profile.historyId` (7.3 step 1),
    because moving it would skip changes outside page 1. A retried first sync (`synced_once`
    still false) takes the new `profile.historyId`, so mail that arrived before the first
    sync completed is never reported as new (7.3 step 4). `Refresh` queues `History` after `InitialSync` for accounts that have
    synced, so a manual refresh also catches up on changes.
  - Mutations and sends for an account that is not signed in are answered at once with an
    error. A mutation that changed at least one thread (an untrash counts once the untrash
    call succeeds, even if the restore after it fails) queues `FetchMeta` for all of its
    threads at the front of the background lane (the authoritative refetch) and the count
    refresh. Send failures go to the composer only, not the banner.
  - `send` failure mapping also counts an unreadable success answer (`Decode`, `TooLarge`)
    as `maybe_sent: true`, since Gmail answered 2xx; a token-endpoint failure is
    `maybe_sent: false`, since nothing was uploaded. `SendResponse` keeps only `threadId`
    (shape still unverified, 15 item 14).
  - `mail::b64::encode` became test-only: the upload send carries raw bytes, so nothing
    outside the tests encodes.
  - Not changed: `known` still grows over a session apart from the burst path and resets.
  - Live steps of 12.4 were not performed by the implementing agent; they are left for the
    user.

### M12: Attachments
- **Files:** `mail/files.rs` (sanitise, unique name, download dir), `sync.rs` `SaveAttachment`
  and Gmail-sourced attachment resolution for Send, the reader's Save button enabled, the
  "Open folder" toast action, the demo notice
- **Acceptance:**
  - The `mail/files` tests pass, and the fake server test gains a save step (inline
    `body.data` and `attachmentId` cases) and a forward-with-attachment send step whose
    rfc822 part carries the original bytes.
  - Saving in demo shows the notice.
  - Live: an attachment saves, if a client is available.

- **M12 notes:**
  - `SyncEvent::Saved(PathBuf)` replaces the save's `Notice` (see 7.5 step 6), and
    `NoticeAction` gained `OpenFolder(PathBuf)` (so it is no longer `Copy`). The event's
    `Debug` prints only the variant name.
  - Save failures are a toast: "Couldn't save <file>: <reason>", where the reason is
    "Attachment too large to download" (`TooLarge`), "The attachment is no longer in Gmail."
    (404, or the part id no longer in the message) or the usual reader message. Network and
    rate-limit failures also go through the 7.6 handling (banner and backoff), as opening a
    thread does. A save for an account that is not signed in is answered at once.
    Attachment data that is not valid base64 is local to that attachment: the save toasts
    "Gmail sent the attachment's data in a form it couldn't read." (a forward fails the same
    way) without the 7.6 backoff or banner, since retrying the account would not help.
  - Forwarded (Gmail-held) attachments are resolved in the order the composer lists them,
    mixed with local ones, against one 25 MB budget; a Gmail part whose declared size alone
    exceeds what is left is refused before it is downloaded. A failed resolution is
    `SendError { maybe_sent: false, message: "Couldn't attach <file>: <reason>" }` and
    nothing is uploaded; like every send failure it goes to the composer, not the banner.
  - The sanitiser also strips U+061C, U+200B–U+200D, U+2060–U+2064 and U+FEFF (invisible
    characters in the same family as the listed ones) and DEL, and trims trailing
    whitespace. A dot more than 32 bytes from the end does not count as an extension when a
    long name is truncated. `$HOME` alone in `user-dirs.dirs` means the home folder; `$HOME`
    followed by anything but `/` is rejected.
  - `start_with` takes a download folder override (`None` in production) so the fake-server
    test saves into a temporary folder.
  - Not yet verified, left for the user: the live Google save, the "Open folder" button
    (`cx.reveal_path`) actually opening the folder, and the real `files::download_dir()`
    lookup of `$HOME` and `$XDG_CONFIG_HOME` (the tests cover the pure parsing and an
    override folder, not the environment lookup). Record the result here once done.

### M13: Notifications and single instance
- **Files:** `notify.rs` (pure part, markup escaping, and the notify-rust `show` with icon
  `APP_ID`), the `main.rs` observer, `instance.rs` (skip entirely when `XDG_RUNTIME_DIR` is
  unset; never fall back to `/tmp`), the `mailto:` hand-off, `cli.rs` positional mailto and
  `--instance`
- **Acceptance:**
  - The `notify` and `instance` tests pass (lock and probe against a temporary directory).
  - With `dbus-monitor` running, the demo's simulated new mail produces exactly one `Notify`
    call after about 20 s, and none on startup.
  - With `--demo --instance`, a second launch started with an xdg-activation token (a
    launcher, a key binding, `niri msg action spawn`) focuses the existing window; one without
    a token hands over and exits, and the compositor decides (see the M13 notes). And
    `eloquawk --demo --instance 'mailto:x@y?subject=Hi&attach=/etc/passwd'` opens a compose
    window in the running instance with the subject set and no attachment.
- **M13 notes:**
  - **Focus needs the launcher's token.** On Wayland a window may only take focus with an
    xdg-activation token the compositor issued for a user action; gpui's `activate_window`
    asks for a token of its own, which niri refuses without recent input to Eloquawk. A second
    launch therefore sends its `XDG_ACTIVATION_TOKEN` as `"token"` with `show` / `compose`, and
    the running instance spends it on the window through `ui/activation.rs`, which wraps gpui's
    `wl_display` and `wl_surface` (raw handles) and sends `xdg_activation_v1.activate`. This
    adds the direct dependencies `raw-window-handle`, `wayland-backend` (`client_system`),
    `wayland-client` and `wayland-protocols` (`client`, `staging`), all at the versions gpui
    already locks, so `Cargo.lock` gains no packages. A launch with no token (a plain terminal
    that sets none) still hands over, but the compositor decides whether to raise the window.
    The acceptance check was run with launches started by `niri msg action spawn`, which
    issues a token as a key binding or launcher does. The criterion above is narrowed to that
    form accordingly. There is no attention fallback for tokenless launches: gpui's
    `request_attention` is a no-op on Wayland at the pinned rev, and `activate_window` already
    asks for a token with the app id (which some compositors show as urgency).
  - `ui/activation.rs` relies on gpui_linux using `wayland-backend` 0.3 with `client_system`
    at the pinned zed rev; recheck it when the rev moves. It does not cache its binding.
  - **First-sync suppression** is the worker's (7.3 step 4, covered by the sync tests); the
    `notify` tests cover what reaches the notifier: a seen map keyed by thread and newest
    message date (a thread notifies again only for a newer message), threads no longer
    unread in INBOX skipped, more than 3 grouped into "N new conversations".
  - The store keeps only `NewMail` summaries filed under the account the event names
    (`new_mail_keeps_only_the_named_accounts_summaries`).
  - A `mailto:` link that arrives before there is an account waits and opens once the first
    account is signed in; once the store knows there is none, a "Sign in to compose this
    message" toast says so, and a hand-off raises the main window to show it.
  - The socket is removed on a normal quit, and the lock released with it, so a launch while
    the old process is still exiting becomes the instance. After a crash or SIGTERM the socket
    is left behind and the next launch, holding the lock, finds nobody answering and replaces
    it. A launch that finds the lock taken retries it while probing, so it takes over if the
    holder exits within the 3 s wait; a holder hung for longer still leaves it running alone.

### M14: Desktop integration and Nix module
- **Files:**
  - `data/korora.tech.Eloquawk.desktop` (`Exec=eloquawk %u`,
    `MimeType=x-scheme-handler/mailto;`, `Categories=Network;Email;`,
    `StartupWMClass=korora.tech.Eloquawk`)
  - `data/branding/{eloquawk-app-icon.svg, eloquawk-app-icon.png, README.md}`. The icon is a
    little penguin with an envelope, in a flat style matching antarctical; the PNG is 512px from
    `rsvg-convert`.
  - `branding.rs` icons, filling the header icon slot and the auth card
  - `nix/hm-module.nix` (`programs.eloquawk.enable` and `package`; **no service**, since there
    is no tray)
  - `flake.nix`: adds `./data` to the fileset and the `postInstall` desktop and icon lines,
    `homeModules.default` plus the `homeManagerModules` alias, and an `hm-module` check
- **As built (M14):** the auth card shows `branding::icon(96)` above it (antarctical shows its
  mascot there; Eloquawk has one icon). `checks` holds `hm-module` (stub evaluation:
  enabled installs the package, disabled installs nothing, `package` is honoured) and
  `desktop-entry` (runs `desktop-file-validate` on the installed entry and checks both icons
  exist). The wrapper also bundles Inter through fontconfig (section 8.3). The desktop entry
  adds `GenericName`, `Keywords` and `StartupNotify`; a unit test ties its `Icon`,
  `StartupWMClass`, `Exec`, `MimeType` and `Categories` to `APP_ID` and this list.
- **Acceptance:**
  - `nix build -L` succeeds and `nix flake check` passes.
  - `result/share/applications/korora.tech.Eloquawk.desktop` and the hicolor icons exist.
  - `desktop-file-validate` passes, if available.

### M15: Documentation and polish
- **Files:**
  - `README.md`, in antarctical's structure: pitch, IMPORTANT OAuth callout, demo hero
    screenshot, what it does, current limits, try without Google, install, keyboard shortcuts,
    troubleshooting, development, licence
  - `docs/google-gmail.md`: creating a Google Cloud project, enabling the Gmail API, the consent
    screen and test users, the restricted scope and the 7-day test-token expiry, the Desktop
    client, pasting it into Settings or building with `ELOQUAWK_GOOGLE_CLIENT_*`, where
    credentials live, how sync works, and which deliveries notify (categories excluded)
  - `docs/desktop-integration.md`: mailto handler (whitelisted fields), notifications, single
    instance, HM module
  - `docs/screenshots/*.webp` from demo mode only, converted with `cwebp`
- **As built (M15):** the README's shortcut table has a row per `SHORTCUTS` entry (keys as
  `help::key_label` shows them, the description, and the context's heading), and
  `help::tests::readme_lists_every_shortcut` fails when the two drift. `GUIDE_URL`
  (`ui/settings.rs`) points at `docs/google-gmail.md` on `main`, and a test checks that file
  exists. Both tests read the checkout, so they skip their file check in the Nix package's
  check phase, whose source holds no docs; CI's `cargo test` runs them in full. The
  needs-client card no longer prints the guide's path in its wrapped copy (it broke as
  "docs /google-gmail.md") and gains a "Setup guide" button that opens `GUIDE_URL`. The
  committed shots are `demo-inbox`, `demo-newsletter-light`, `demo-reply`, `demo-search` and
  `demo-shortcuts`, scaled to half the 2x capture with `cwebp -q 82 -resize 1693 0`. AI
  triage is not part of v1 (korora-tech/eloquawk#1); the README lists it as planned.
- **Acceptance:**
  - Every README shortcut matches `ui/help.rs`.
  - `rg -n 'allow\(dead_code\)' src` is empty, or each remaining hit is justified.
  - No non-test `{:?}` of mail types (`rg -n ':\?\}' src` reviewed by hand).
  - Final adversarial review of the whole tree against this spec.
  - All shots retaken.

---

## 14. Non-goals for v1

- Rendering HTML mail.
- Loading remote images. They are never loaded at all.
- A tray icon or background service.
- Offline mail cache on disk.
- Editing and syncing drafts. The Drafts label lists them read-only, and opening one shows it.
- Send-as aliases and signatures.
- Snooze, scheduled send, filters and label management (create, rename, delete).
- Permanent delete, which needs a broader scope.
- A unified inbox across accounts.
- Resizable panes.
- Gmail batch HTTP.
- Address autocomplete and contacts.
- Selecting text across several message cards at once.
- A "Save as…" dialog for attachments (they always go to the download directory).

The architecture leaves room for each of these: new `Command`/`SyncEvent` variants and new
views.

## 15. Open items to VERIFY during implementation

1. Charset of `body.data` under `format=full`. Decode defensively. **M2:** `mail/parse.rs`
   takes bytes that are valid UTF-8 as UTF-8 whatever the declared charset (in case Gmail
   already converted them), and otherwise decodes with the declared charset, falling back to
   windows-1252. Still to confirm against a live non-UTF-8 message.
2. Whether `has_attachments` can be derived from `format=metadata`, using `payload.mimeType ==
   multipart/mixed` as a heuristic. If not, fall back to false and fill it in from the detail.
   **M2:** `thread_summary` uses the heuristic, plus any attachment part when the parts tree is
   present. Still to confirm what `format=metadata` returns live.
3. ~~Key name for `#` and `!`~~ **Resolved:** bind `#`, `!`, `?`, `:` as characters (section 9).
   **M6:** `shift-space` keeps its shift: gpui_linux clears shift only for single-character
   keys, and space is reported as the named key `space`.
4. ~~`cx.prompt_for_paths` with `default-features = false`~~ **Resolved:** compiled in through
   ashpd with the wayland/x11 features; it can fail at runtime without a portal (handled).
5. The exact niri `screenshot-window` flags. **Checked at M1:** `--id`, `--path` (absolute)
   and `--write-to-disk true` exist as written in 12.3. While the session has no focused
   window (for example, locked behind a layer-shell surface), niri writes no file and ignores
   `close-window`, for antarctical as well; the script must treat a missing PNG as "not
   verified", not as a pass.
6. That the html2text, lettre and mail-parser versions build in the gpui tree with no tokio.
   **Checked at M2** for html2text (used) and the whole lock: `cargo tree -i tokio` is empty.
7. The INBOX `threadsUnread` semantics compared with what the Gmail web UI shows.
8. The label effects of `threads.trash` (does it strip INBOX?) and `threads.untrash` (does it
   restore INBOX?). The optimistic model and undo's `restore` depend on it.
9. `threads.list` with `labelIds=TRASH` / `SPAM` and `includeSpamTrash=true`.
10. The html2text 0.17 custom decorator API for `InlineLinks`. **Resolved at M2:**
    `decorate_link_end` cannot see the link text, so `InlineLinks` emits no markup and only
    annotates link spans; `lines_from_read` returns the annotated lines and `mail/html.rs`
    joins them, writing `text <href>` or the bare href (a loose match such as `example.com`
    for `https://example.com/` shows the href). Relative and `#` links show their text only.
    Lists use an ASCII `- ` bullet, because html2text 0.17 subtracts the prefix's byte length
    from a width estimate and underflows on `• `. Rendering uses a 10 000-column width so
    paragraphs are not hard-wrapped, and runs of 3+ spaces (table column padding) shrink to
    two. Known limitation: html2text prunes a link with no text (an image-only banner link)
    before the decorator sees it, so such a link disappears with its image.
11. That `Keystroke::parse("j")` sets `key_char`, and that `window.dispatch_keystroke` falls
    through to the focused input handler when no binding matches (needed for `type:` and the
    search test).
12. The ureq 3.5 per-request timeout call.
13. ~~The `ScrollStrategy` variant names~~ **Resolved at M4:** `Top`, `Center`, `Bottom`,
    `Nearest`; the list uses `Nearest`.
14. The response shape of `messages.send` with `uploadType=multipart`.

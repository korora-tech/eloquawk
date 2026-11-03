# message-checks Specification

## Purpose
Flag phishing, scams and spam with TypeSafe's Jev model through OpenRouter, or Cloudflare's
Clef or Clef-flash on Workers AI, for accounts the user opts in, while keeping what leaves the machine small and redacted, and give every
account a local caution and link guard that need no network. Checks are advisory only.
Implementation detail lives in docs/SPEC.md section 16 (except 16.8, with Clef in 16.17) and
milestone M16, and in docs/ai-triage.md. Label suggestions belong to their own capability.

## Requirements

### Requirement: Off until a key and per-account consent, revocable at once
The app SHALL send nothing to the chosen checker unless its key is saved (and, for Clef, a
valid account ID is set) and the account has accepted the current disclosure. Settings SHALL
offer one "Check messages in <email>" toggle per account; turning it on SHALL first open an
inline disclosure of what is sent, with Turn on and Cancel, and links to the privacy policies
of the processors involved: OpenRouter and TypeSafe for Jev, Cloudflare for Clef. When what
is sent changes, every account SHALL count as off and SHALL show "What Eloquawk sends has
changed. Review and turn on again." until the user accepts again. Without a key the toggles
SHALL be disabled with the caption "Add a key first".
Turning an account off, removing it, Gmail resetting its history, an outdated disclosure,
switching checker and removing the key SHALL drop every queued and retrying check they cover
before another request starts, and SHALL forget their results. An answer to a request
already sent SHALL be discarded.

#### Scenario: No account opted in
- **WHEN** a key is saved, no account is opted in, and the user opens messages
- **THEN** no check is sent

#### Scenario: Turning an account on
- **WHEN** the user ticks "Check messages in <email>" for the first time
- **THEN** the disclosure opens under the toggle and the account stays off
- **AND** only Turn on opts the account in

#### Scenario: Disclosure changed since consent
- **WHEN** the app starts with an account that accepted an older disclosure
- **THEN** the account's toggle is off with the "What Eloquawk sends has changed" caption
- **AND** nothing queued under the old consent is sent

#### Scenario: Opt out while a check is queued
- **WHEN** the user turns an account off while one of its checks is waiting
- **THEN** that check is never sent, and any answer already on its way shows nothing

### Requirement: Choosing the checker
Settings SHALL offer a checker choice of "Jev (OpenRouter)", "Clef (Cloudflare)" and
"Clef-flash (Cloudflare)", defaulting to Jev, with the caption "Switching checker turns checks
off for every account until you turn them on again." Choosing a different checker SHALL turn
every account off, drop every queued and retrying check, forget every result of the session,
lift any pause, and show the chosen checker's key status. Only the chosen checker SHALL ever
receive a request. Texts that name the processor or the model SHALL name the chosen one: the
card intro, the disclosure and its privacy links, the key status, the Get a key link, errors,
the Test result, the reader banner, and the label guidance caption.

#### Scenario: Default checker
- **WHEN** the app starts with no saved checker
- **THEN** Jev (OpenRouter) is chosen and behaves as before

#### Scenario: Switching to Clef
- **WHEN** an account is opted in with Jev and the user chooses Clef (Cloudflare)
- **THEN** the account's toggle is off, no earlier banner or badge remains, and nothing is
  sent until the user accepts the Clef disclosure

#### Scenario: Disclosure names Cloudflare
- **WHEN** Clef is chosen and the user ticks "Check messages in <email>"
- **THEN** the disclosure names Cloudflare and Clef and links to Cloudflare's privacy policy

### Requirement: Clef requests go only to Cloudflare Workers AI
With a Clef checker every check SHALL go to
`https://api.cloudflare.com/client/v4/accounts/<account id>/ai/run/@cf/cloudflare/clef`, or
`.../clef-flash` for Clef-flash, with the selector `clef` or `clef-flash` in the body and
no other routing options. The account ID SHALL be 32 lowercase hexadecimal characters, saved
in the settings file with Set, and refused with "That doesn't look like a Cloudflare account
ID (32 hexadecimal characters)" otherwise. Without a valid account ID the account toggles SHALL be
disabled with "Add your Cloudflare account ID first". Each check's cost SHALL be counted as
its reported input tokens at $0.24 per million. Redirects SHALL never be followed, and no
setting, flag or environment variable SHALL change the host.

#### Scenario: Account ID refused
- **WHEN** the user sets the account ID "../../evil"
- **THEN** the inline error shows and the saved account ID is unchanged

#### Scenario: Cost from tokens
- **WHEN** Clef answers a check with 1,000 input tokens
- **THEN** the session usage grows by $0.00024

### Requirement: Checker keys live only in the Secret Service and can be tested
Keys SHALL be stored only in the Secret Service under the service `korora.tech.Eloquawk`: the
OpenRouter key as item `openrouter-api-key` labelled "Eloquawk: OpenRouter API key", and the
Cloudflare API token as item `cloudflare-api-token` labelled "Eloquawk: Cloudflare API
token". The key field SHALL be masked and cleared on Save, on Remove and when Settings
closes; afterwards the app SHALL show only a hint such as "sk-or-…abcd" or "…abcd". An
OpenRouter key that does not start with `sk-or-`, is not 20 to 256 characters, or contains
whitespace or control characters SHALL be refused with "That doesn't look like an OpenRouter
key (it should start with sk-or-)". A Cloudflare token that is not 32 to 256 letters, digits,
`_` or `-` SHALL be refused with "That doesn't look like a Cloudflare API token". A refused
key SHALL NOT be saved. The keyring SHALL NOT be read for a key until the app's own first
keyring access has settled. Remove SHALL delete the chosen checker's key, drop every queued
check and forget every result.
Test SHALL check the typed key, or the saved one when the field is empty, by asking the
chosen model one synthetic question that contains no mail, after reading the key's remaining
credit for OpenRouter. Success SHALL read "Key works · <model> answered in <n> s", followed by
"· $<x> left on this key" when an OpenRouter key has a credit limit, and "· Save it to use
it" for a typed, unsaved key. Failure SHALL show the error's user-facing text.

#### Scenario: Saving a key
- **WHEN** Jev is chosen and the user pastes a valid key and presses Save
- **THEN** the field empties and the status reads "Saved (sk-or-…<last four>)"

#### Scenario: Saving a Cloudflare token
- **WHEN** Clef is chosen and the user pastes a valid token and presses Save
- **THEN** the field empties and the status reads "Saved (…<last four>)"

#### Scenario: Invalid key
- **WHEN** Jev is chosen and the user saves "hello"
- **THEN** the inline error asks for a key starting with sk-or- and nothing is stored

#### Scenario: Key status before the keyring answers
- **WHEN** Settings opens before the key has been read
- **THEN** the status reads "Checking the keyring…" and the account toggles are disabled

#### Scenario: Removing the key
- **WHEN** the user presses Remove
- **THEN** the status reads "No key saved", and no banner, badge or caption from earlier
  checks remains

#### Scenario: Testing a typed key
- **WHEN** the user types a working key and presses Test without saving it
- **THEN** the result ends "Save it to use it" and any pause stays in place

### Requirement: Only a redacted extract of one message is sent
Apart from what label suggestions add, a check SHALL send only these fields of the one
message being checked: the sender's name and address, up to 5 Reply-To addresses, the
Return-Path domain, the subject (300 chars),
Gmail's own SPF, DKIM and DMARC results with up to 5 DKIM signing domains, whether a
`List-Unsubscribe` header is present, up to 40 links as display text (80 chars) and
destination host, up to 10 attachment types (extension and MIME type), and up to 6,000
characters of the body without its folded quote. Before sending, every web link SHALL become
`[link: <host>]`, the user's and the recipients' addresses SHALL become `[your address]` and
`[recipient]`, any other address `[email: <domain>]`, and a 4 to 8 digit code near an
English code word such as "code", "OTP", "PIN" or "verification" SHALL become `[code]`. The
app SHALL NOT send the To, Cc or Bcc fields, any URL path or query, attachment names or
contents, other messages in the thread, or Gmail ids. Names, phone numbers and other text
SHALL be sent as written, and the disclosure SHALL say so.

#### Scenario: Hostile message
- **WHEN** a checked message holds a password-reset URL with a token, the account's own
  address in mixed case, a Cc address and "Your verification code is 123 456"
- **THEN** the request holds `[link: <host>]`, `[your address]`, `[recipient]` and `[code]`
- **AND** it holds no URL path, no `?`, no token, neither address and not `123 456`

#### Scenario: Link shown as an address
- **WHEN** a link's display text is itself a URL
- **THEN** only that text's host is sent as the display text

### Requirement: Jev requests go only to OpenRouter, without retention
With Jev chosen every check SHALL go to OpenRouter's Decisions API with the pinned model
`typesafe/jev-1.13` and SHALL ask for zero data retention, no data collection and a prompt
price of at most $0.42 per million tokens, so routing fails rather than reach any other
endpoint. Redirects SHALL never be followed, and no setting, flag or environment variable
SHALL redirect the key or mail elsewhere. The disclosure, the Settings intro and
docs/ai-triage.md SHALL say that the OpenRouter account's own logging and Broadcast
settings still apply and recommend a dedicated key with them off.

#### Scenario: No endpoint meets the constraints
- **WHEN** OpenRouter answers that no endpoint meets the provider constraints, or answers
  with a redirect
- **THEN** checks pause with "Jev isn't available with zero data retention within
  Eloquawk's price limit right now" and mail sync is unaffected

### Requirement: Which message is checked and when
The checked message SHALL be the last message in the thread that is not a draft, not sent
and not from any signed-in account; a thread with no such message SHALL never be sent. An
opted-in thread's message SHALL be checked when the user opens it, at most once per session:
reopening SHALL use the remembered answer, and moving quickly through threads SHALL cost at
most the check in flight and the last one opened. Turning an account on SHALL check the
already open message at once.

#### Scenario: Only my own messages
- **WHEN** the user opens a thread whose messages are all sent by the user
- **THEN** nothing is checked

#### Scenario: Fast j/k
- **WHEN** the user moves through ten threads with `j` while a check is in flight
- **THEN** at most two checks are made: the one in flight and the last thread opened

#### Scenario: Reopening
- **WHEN** the user reopens a message checked earlier in the session
- **THEN** its result shows again with no new request

### Requirement: Verdict rules
A phishing or scam score of at least 0.50 SHALL give a warning and at least 0.80 danger; the
higher level SHALL win. When phishing and scam reach the same level, the higher score SHALL
name the verdict, and an exact tie SHALL name phishing. A spam score of at least 0.85 SHALL
give a spam verdict only when neither phishing nor scam reaches a warning; spam SHALL never
be shown as a warning or danger. A clean result SHALL show nothing, and the app SHALL never
show a "safe" or "verified" mark.

#### Scenario: Inheritance scam that also scores as phishing
- **WHEN** a message scores phishing 0.97 and scam 0.99
- **THEN** the verdict is danger, named as a scam

#### Scenario: Thresholds
- **WHEN** a message scores phishing 0.79, scam 0.10 and spam 0.90
- **THEN** the verdict is a phishing warning, not spam

### Requirement: Reader banner
The reader SHALL show a banner below the subject for a warning, danger or spam verdict on
the open thread's checked message. Its title SHALL be "This looks like phishing" or "This
looks like a scam" (danger), "This may be phishing" or "This may be a scam" (warning), or
"This looks like spam". The right-hand text SHALL be qualitative ("<model>: very likely",
"<model>: possible" or "<model>: likely spam", where the model is Jev, Clef or Clef-flash),
adding "· also possibly a scam" or "· also possibly phishing" when the other score is at
least 0.50, with a tooltip "Model score <n>, not a probability of harm". The reasons line
SHALL hold up to three of Eloquawk's own observations (a failed DMARC, or SPF or DKIM when
DMARC is unknown; the first link whose text names another host; a Reply-To in another
domain; a risky attachment type), or "<model>'s assessment of the content" when there are
none, with every host in its ASCII (punycode) form. The banner SHALL offer Report spam, which
is undoable, and Dismiss, which hides it for the session. The app SHALL never act on a
verdict by itself.

#### Scenario: Phishing banner
- **WHEN** the user opens a fake PayPal message that failed DMARC and links "www.paypal.com"
  to paypa1-verify.example, and Jev scores it as phishing danger
- **THEN** the banner reads "This looks like phishing" with "Sender failed DMARC · Link text
  shows www.paypal.com but goes to paypa1-verify.example"

#### Scenario: Spam view
- **WHEN** the user opens a spam-level message from the Spam view
- **THEN** no spam banner shows, while phishing and scam banners still show there without
  Report spam

#### Scenario: Dismiss
- **WHEN** the user clicks Dismiss
- **THEN** the banner stays hidden for that message for the rest of the session

#### Scenario: Clef wording
- **WHEN** Clef is chosen and scores a message as phishing danger
- **THEN** the banner's right-hand text reads "Clef: very likely"

### Requirement: Thread-row badge
A thread checked this session SHALL show a pill on its row: "Phishing" or "Scam" for danger,
"Suspicious" for a warning, and "Spam" for spam except in the Spam view. Clean and unchecked
threads SHALL show no badge.

#### Scenario: Badge after opening
- **WHEN** the user opens a scam that scores danger and returns to the list
- **THEN** its row shows "Scam"

### Requirement: Failures and pauses
After a failed check of an opted-in message with no banner, the reader SHALL show the muted
caption "Couldn't check this message", with "· Retry" when a retry may help. Busy and network
errors SHALL be retried with backoff (at most two retries on open) before failing. A rejected
key, no credit or allocation, a key, budget or permission limit, an unavailable endpoint and
the spend limit SHALL pause all checks for the session: the reader SHALL show "Paused:
<reason>" and Settings "Checks are paused: <reason>" with Try again. A rejected key SHALL
stay paused until a key is saved or the saved key passes Test, and Try again SHALL NOT be
offered for it. Mail sync SHALL never be affected. With Clef the reasons SHALL read
"Cloudflare rejected this token", "Your Workers AI allocation has run out", "Cloudflare
refused the request (token permissions or account ID)", "Clef isn't available on Workers AI
right now" and "Couldn't reach Cloudflare".

#### Scenario: Retryable failure
- **WHEN** the checker stays busy through every attempt of a check on open
- **THEN** the reader shows "Couldn't check this message · Retry"
- **AND** Retry sends the check again

#### Scenario: Credit runs out
- **WHEN** OpenRouter reports that the key's credit has run out
- **THEN** the reader shows "Paused: Your OpenRouter credit has run out" and no further
  checks are sent until Try again

#### Scenario: Rejected key
- **WHEN** OpenRouter rejects the key
- **THEN** the key status reads "OpenRouter rejected this key" and the key is not sent again
  until a key is saved

#### Scenario: Rejected Cloudflare token
- **WHEN** Clef is chosen and Cloudflare answers 401
- **THEN** the key status reads "Cloudflare rejected this token" and checks pause

### Requirement: Session spend limit and usage
Settings SHALL hold a per-session spend limit, default $1.00, accepting $0.10 to $100 on
Enter or on leaving the field, and otherwise showing "Enter an amount from $0.10 to $100"
and keeping the old value. When the session's reported cost reaches the limit, checks SHALL
pause with "Session spend limit reached ($<limit>)", and Try again SHALL allow another
limit's worth. A check whose cost is not reported SHALL count as $0.001. Lowering the limit
below what the session has spent SHALL pause at once. Settings SHALL show "This session: N
checks · $<cost> of $<limit>", adding "· cost unknown for M" when M > 0; usage SHALL NOT be
saved.

#### Scenario: Limit reached
- **WHEN** the session's checks reach the $1.00 limit
- **THEN** checks pause with "Session spend limit reached ($1.00)"
- **AND** Try again resumes checks until another $1.00 is spent

#### Scenario: Out-of-range limit
- **WHEN** the user enters "500" as the limit
- **THEN** the error "Enter an amount from $0.10 to $100" shows and the limit is unchanged

### Requirement: Background checks of new mail
One global toggle, "Check new inbox mail in the background", off by default, SHALL send each
new inbox message of opted-in accounts for checking as it arrives, including messages never
opened, so their badges show in the list. It SHALL be capped at 60 messages per account per
rolling hour, SHALL NOT mark mail read, and SHALL NOT report its fetch failures in the sync
problems banner. Whether to send SHALL be decided again when the message arrives, so turning
the toggle off, opting out or losing the key in between sends nothing.

#### Scenario: New mail checked without opening
- **WHEN** background checks are on and a phishing message arrives in an opted-in inbox
- **THEN** its row shows a badge before the user opens it, and it stays unread

#### Scenario: Toggled off before the fetch lands
- **WHEN** the user turns background checks off while a new message is being fetched
- **THEN** that message is not sent for checking

### Requirement: Local caution for every account
When the checked message failed DMARC and has a link whose text names a different host than
its destination, the reader SHALL show the amber row "Eloquawk noticed: Sender failed DMARC ·
Link text shows <text host> but goes to <host>" for any account, opted in or not, with no
key and no request, whenever no warning or danger banner shows. It SHALL have no score, no
Jev wording and no Dismiss.

#### Scenario: Injected "this email is safe"
- **WHEN** a message that failed DMARC with a mismatched link says "This email is safe" and
  Jev scores it clean
- **THEN** the local caution still shows

#### Scenario: Account not opted in
- **WHEN** the user opens such a message in an account that is not opted in
- **THEN** the local caution shows and nothing is sent to OpenRouter

### Requirement: Phishing link guard
While the open thread's checked message has a danger verdict or a local caution, clicking a
web link in that message SHALL NOT open it; the app SHALL instead post "This message looks
like phishing. Open <punycode host>?" with an Open link button. `mailto` links SHALL open the
composer as usual, and links in other messages of the thread SHALL open as usual. The guard
is best effort: a click before Jev answers SHALL be guarded only by the local caution.

#### Scenario: Guarded link
- **WHEN** the user clicks a web link in a message showing the local caution
- **THEN** the browser does not open and the notice names the link's punycode host
- **AND** Open link opens it

#### Scenario: Homograph host
- **WHEN** a guarded link goes to a Cyrillic look-alike of paypal.com
- **THEN** the notice shows the `xn--` form of the host

### Requirement: Logs carry no mail or key
Message-check logs SHALL be limited to the HTTP status, the checker's error code (with
OpenRouter also its `metadata.reason` or `limit_source`), the network error kind and keyring
failures, prefixed `eloquawk: jev:`. With `ELOQUAWK_JEV_DEBUG=1` the app MAY also log check
decisions, the key state and label names and scores. No log SHALL contain the key, the
account ID, a request or response body, the checker's error message, a subject, an address or
a host.

#### Scenario: Failed request
- **WHEN** a check fails with a 402
- **THEN** stderr shows the status and error code and no message text

### Requirement: Demo mode is offline
In demo mode checks SHALL run through a built-in offline checker whatever checker is chosen:
no network request and no keyring access. Both demo accounts SHALL start opted in with
background checks on, Settings SHALL replace the key row with "Demo mode uses a built-in
offline checker. Nothing is sent.", and Test SHALL answer "Demo mode: the checker is
offline".

#### Scenario: Demo phishing message
- **WHEN** the user opens "Your account will be suspended" in `--demo`
- **THEN** the phishing danger banner shows and no connection is made

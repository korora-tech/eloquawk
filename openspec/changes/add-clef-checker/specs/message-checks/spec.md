## ADDED Requirements

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

## MODIFIED Requirements

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

## RENAMED Requirements
- FROM: `### Requirement: The OpenRouter key lives only in the Secret Service and can be tested`
- TO: `### Requirement: Checker keys live only in the Secret Service and can be tested`
- FROM: `### Requirement: Requests go only to OpenRouter, without retention`
- TO: `### Requirement: Jev requests go only to OpenRouter, without retention`

# demo-mode Specification

## Purpose
Run Eloquawk against a generated mailbox with no Google account, so the whole UI can be tried,
scripted and screenshotted without touching the user's mail, keyring or network. Implementation
detail lives in docs/SPEC.md sections 11, 12.3 and 16.13. What the offline message checker
answers belongs to the message-checks capability.

## Requirements

### Requirement: Entering demo mode
`--demo`, or the environment variable `ELOQUAWK_DEMO=1`, SHALL start the app in demo mode.
`--demo-actions` without demo mode SHALL be refused with "--demo-actions needs --demo" and
exit status 2.

#### Scenario: Environment switch
- **WHEN** the user runs `ELOQUAWK_DEMO=1 eloquawk`
- **THEN** the app shows the generated mailbox

#### Scenario: Script without demo
- **WHEN** the user runs `eloquawk --demo-actions "j"`
- **THEN** the app prints "--demo-actions needs --demo" with the usage and exits with status 2

### Requirement: Offline and private
In demo mode the app SHALL NOT contact Google or OpenRouter and SHALL NOT open the Secret
Service. It SHALL read the saved settings but never write them, keep no summary cache and
save no attachments. Sign-in, client and account commands SHALL be refused with the notice
"Demo mode: sign-in is disabled". Message checks SHALL use a built-in offline checker.
Desktop notifications SHALL still be shown.

#### Scenario: Settings change in demo
- **WHEN** the user picks Light in Settings in demo mode and relaunches without `--demo`
- **THEN** the theme is the one saved before the demo run

#### Scenario: No keyring prompt
- **WHEN** the app starts in demo mode with the keyring locked
- **THEN** no unlock prompt appears and the generated mailbox shows

### Requirement: Generated mailbox
Demo mode SHALL show the same generated mailbox on every run, with dates relative to the
current time: the accounts you@example.com (about 120 threads) and you@work.example (about
40), the system labels plus "Receipts", "Travel", "Projects", "Projects/Eloquawk" and
"Projects/Website". It SHALL include unread and starred threads, threads with attachments, an
HTML newsletter with a link whose text differs from its target, a deep quote chain, a body
with a one-time code and a tracking number, a plain body over 200 KB, non-ASCII names and
subjects such as "Zoë Ångström" and "Café ☕ plans", threads in Spam and Trash, and dates from
minutes to about 400 days ago.

#### Scenario: Two accounts
- **WHEN** the app starts in demo mode
- **THEN** the sidebar lists you@example.com and you@work.example and the Inbox has unread mail

#### Scenario: Deterministic content
- **WHEN** the same demo script runs twice
- **THEN** it sees the same threads in the same order both times

### Requirement: Simulated mail actions
Demo mode SHALL apply archive, trash, restore, star, read, label and spam changes in memory
after a short delay, as Gmail would. A change to a thread whose subject contains "[fail]"
SHALL fail, so the rollback can be seen. Sending SHALL add the message to Sent and to its
conversation, or start a new one. Refresh SHALL reload the labels and the current view.

#### Scenario: Failing thread
- **WHEN** the user archives a thread whose subject contains "[fail]"
- **THEN** the thread returns to the list and an error notice shows

#### Scenario: Sent reply
- **WHEN** the user replies to a demo conversation and sends
- **THEN** the conversation gains the reply and it is listed in Sent

### Requirement: Simulated new mail
About 20 seconds after start, demo mode SHALL deliver one new unread Inbox thread to
you@example.com, "Are you free for a quick call?" from Priya Raman, and notify about it as
real new mail would.

#### Scenario: New mail arrives
- **WHEN** a demo script waits 21 seconds after start
- **THEN** the Inbox shows "Are you free for a quick call?" as unread and a desktop
  notification has been sent

### Requirement: Single instance in demo mode
A demo run SHALL NOT take part in the single instance unless `--instance` is given; with it,
it SHALL use names of its own so that it never hands off to or receives from a normal
Eloquawk.

#### Scenario: Demo beside a real instance
- **WHEN** a normal Eloquawk is running and the user starts `eloquawk --demo`
- **THEN** a second, demo window opens and the running instance is untouched

### Requirement: Scripted steps
`--demo-actions` SHALL take comma-separated steps and run them after the first mail list
arrives, one every 250 ms, against the focused window (the newest compose window, else the
main window, when no window has focus). The steps SHALL be `wait:N` (milliseconds),
`theme:light` and `theme:dark`, `shot`, `open:N` (cursor to row N and open it), `act:<Name>`
(dispatch a named action), `type:<text>` (one keystroke per character), `expect:<cond>`,
`selectall:N`, `view:<label id>` (`ALL_MAIL` for All Mail), `search:<query>`, `attach:<path>`,
`save:N`, `settings:ai` and `link:N`; anything else SHALL be read as keystrokes separated by
spaces, with `comma` for `,`, so `g i` sends `g` then `i`. In `type:`, `search:` and `expect:`
values `\,` SHALL stand for a comma. A step that cannot be read SHALL print `ELOQUAWK_EXPECT
fail step <step>` and be skipped. `link:N` SHALL print `link: would open <host>` instead of
opening a browser.

#### Scenario: Keys and actions
- **WHEN** the app runs with `--demo --demo-actions "j,j,o,expect:open=2"`
- **THEN** it prints `ELOQUAWK_EXPECT ok open=2`

#### Scenario: Unknown step
- **WHEN** a script holds the step `frobnicate`
- **THEN** the app prints `ELOQUAWK_EXPECT fail step frobnicate` and runs the next step

#### Scenario: Escaped comma
- **WHEN** a script holds `search:from:zoë\, subject:plans`
- **THEN** the search field holds `from:zoë, subject:plans`

### Requirement: Expectations
`expect:<cond>` SHALL print `ELOQUAWK_EXPECT ok <cond>` when the condition holds and
`ELOQUAWK_EXPECT fail <cond> (actual <value>)` when it does not. The conditions SHALL cover
the list (`cursor`, `rows`, `selected`, `unread`, `starred`, `label`, `view`, `account`,
`badge`, `syncs`), the reader (`open`, `card`, `expanded`, `scroll`, `selection`, `messages`),
focus and overlays (`focus`, `search`, `help`, `picker`, `filter`, `screen`, `notice`), compose
windows (`compose`, `compose-error`, `attached`, `discard`, `composers`) and message checks
(`ai`, `suggest`). A focused compose window SHALL answer focus and its own conditions; the
main window SHALL answer the rest.

#### Scenario: Failing expectation
- **WHEN** a script runs `expect:cursor=5` with the cursor on row 2
- **THEN** the app prints `ELOQUAWK_EXPECT fail cursor=5 (actual 2)`

#### Scenario: Compose expectations
- **WHEN** a script runs `c,expect:compose=to,expect:composers=1`
- **THEN** both expectations print `ok`

### Requirement: Ready line for screenshots
`shot` SHALL print `ELOQUAWK_READY <title>` for the focused window and flush it at once. The
title SHALL be "Eloquawk" for the main window and the compose window's own title for a
compose window.

#### Scenario: Compose shot
- **WHEN** a script runs `c,shot`
- **THEN** the app prints `ELOQUAWK_READY New message`

### Requirement: Screenshot script
`scripts/screenshot.sh <name> [demo-actions] [theme]` SHALL build the app, run it in demo mode
with the theme (default dark) and the steps (default `wait:1500`) followed by `shot`, wait up
to 60 seconds for the ready line, capture the window with that title on niri to
`target/shots/<name>.png`, print every `ELOQUAWK_EXPECT` line and stop the app. It SHALL exit
non-zero when an expectation failed, the app never became ready or the capture failed, and
SHALL leave the clipboard as it found it.

#### Scenario: Settings shot
- **WHEN** the user runs `scripts/screenshot.sh settings ctrl-comma`
- **THEN** `target/shots/settings.png` shows the Settings screen and the script exits 0

#### Scenario: Failed expectation
- **WHEN** the steps include an expectation that fails
- **THEN** the shot is still taken and the script exits non-zero

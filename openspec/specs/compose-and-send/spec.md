# compose-and-send Specification

## Purpose
Write new messages, replies, reply-alls and forwards in their own window, and send them
through Gmail with correct recipients, quoting and threading. Implementation detail lives in
docs/SPEC.md sections 8.5, 8.6 and 10 and milestone M8. Attachments in the composer belong
to the attachments capability.

## Requirements

### Requirement: Opening a compose window
The app SHALL open each message in its own compose window. `c`, ctrl-n and the sidebar's
Compose SHALL open a new message from the current account; `r`, `a` and `f` and the
reader's Reply, Reply all and Forward buttons SHALL answer the open conversation, else the
cursor row. A new message's window SHALL be titled "New message"; a reply's or forward's
window SHALL be titled with its subject. Focus SHALL start in To for a new message or a
forward, and in the body for a reply or reply-all.

#### Scenario: New message
- **WHEN** the user presses `c` in the mail view
- **THEN** a window titled "New message" opens with focus in To

#### Scenario: Reply opens on the body
- **WHEN** the user opens a conversation and presses `r`
- **THEN** a compose window titled with "Re: " and the subject opens with focus in the body

#### Scenario: Conversation not loaded yet
- **WHEN** the user presses `f` on a cursor row whose conversation has not loaded
- **THEN** the conversation opens in the reader and the compose window follows once it loads
- **AND** if it fails to load, the app shows the notice "Couldn't load the conversation"

#### Scenario: Nothing to answer
- **WHEN** the user presses `r` with no open conversation and no cursor row
- **THEN** the app shows the notice "No conversation to reply to" and opens no window

#### Scenario: No account
- **WHEN** the user presses ctrl-n with no signed-in account
- **THEN** the app shows the notice "Add an account to write mail"

### Requirement: Compose fields
The compose window SHALL offer To, Cc, Bcc, Subject and the body. Cc and Bcc SHALL stay
behind a "Cc Bcc" link unless the prefill fills them. With two or more accounts it SHALL
show a From selector, defaulting to the current account for a new message and to the
conversation's account for a reply or forward. Enter in a one-line field SHALL move to the
next field, tab and shift-tab SHALL move between fields, and a line break typed or pasted
into a one-line field SHALL become a space.

#### Scenario: Reveal Cc and Bcc
- **WHEN** the user clicks "Cc Bcc" in a new message
- **THEN** the Cc and Bcc fields appear

#### Scenario: Enter in the subject
- **WHEN** the subject field is focused and the user presses Enter
- **THEN** focus moves to the body and the subject stays on one line

#### Scenario: Replying from another account
- **WHEN** the user switches From to a different account on a reply
- **THEN** the sent message is not filed into the original conversation's thread
- **AND** it still carries In-Reply-To and References for the message it answers

### Requirement: Recipient validation
Each address field SHALL show a red border while any entry in it is not a valid address.
Send SHALL stay disabled until To, Cc and Bcc together hold at least one address and none
is invalid.

#### Scenario: Invalid address
- **WHEN** the user types "bob@" in To
- **THEN** the To field has a red border and Send is disabled

#### Scenario: Bcc only
- **WHEN** To and Cc are empty and Bcc holds one valid address
- **THEN** Send is enabled

### Requirement: Body editing
The body SHALL be a multi-line editor in which Enter inserts a newline, paste keeps the
pasted line breaks, ctrl-z undoes and ctrl-shift-z or ctrl-y redoes, ctrl-a selects all,
shift extends the selection for every cursor move, ctrl-left and ctrl-right move by word,
and the mouse selects by click and drag. It SHALL accept IME input. It SHALL scroll itself
to keep the cursor visible. ctrl-enter in the body SHALL send rather than insert a newline.

#### Scenario: Multi-line paste
- **WHEN** the user pastes text holding three lines into the body
- **THEN** the body shows three lines

#### Scenario: Typing past the bottom
- **WHEN** the user types more lines than the body can show
- **THEN** the body scrolls so the cursor stays visible

#### Scenario: Undo a typing run
- **WHEN** the user types a word and presses ctrl-z
- **THEN** the word is removed

### Requirement: Reply and reply-all recipients
A reply or forward SHALL answer the conversation's last message that is not a draft. A reply
SHALL go to that message's Reply-To, else its sender; when the user sent that message, the
reply SHALL go to its recipients instead. Reply-all SHALL add the message's To to the reply's
To and its Cc to Cc, SHALL leave out the user's own address, and SHALL list no address twice,
comparing addresses without regard to case; an address in To SHALL NOT appear again in Cc. A
forward SHALL start with no recipients.

#### Scenario: Reply-To wins
- **WHEN** the user replies to a message from ann@example.com with Reply-To list@example.com
- **THEN** To holds list@example.com only

#### Scenario: Reply to my own message
- **WHEN** the last message in the conversation was sent by the user to bob@example.com
- **THEN** a reply goes to bob@example.com

#### Scenario: Reply-all without duplicates
- **WHEN** the user replies all to a message from Ann to the user and Bob, copied to bob@example.com
  and Cleo
- **THEN** To holds Ann and Bob, Cc holds Cleo only, and the user's own address appears nowhere

### Requirement: Subject prefixes and quoted text
A reply's subject SHALL get "Re: " unless it already starts with "re:" in any case; a
forward's SHALL get "Fwd: " unless it already starts with "fwd:" or "fw:". A reply body SHALL
open with two empty lines, then "On <date>, <Name> <<address>> wrote:", then the answered
message's text with each line prefixed by "> " (an empty line by ">"), with the cursor at the
top. A forward body SHALL open with two empty lines, then "---------- Forwarded message
---------" and From, Date, Subject, To and, when present, Cc lines, then an empty line and the
original text. Dates SHALL follow the user's clock preference.

#### Scenario: Existing prefix
- **WHEN** the user replies to "RE: Lunch"
- **THEN** the subject stays "RE: Lunch"

#### Scenario: Forward of a reply
- **WHEN** the user forwards "Re: Lunch"
- **THEN** the subject is "Fwd: Re: Lunch"

#### Scenario: Quoted reply
- **WHEN** the user replies to Zoë's message "Shall we?", an empty line, then "Noon works."
- **THEN** the body ends with "wrote:" followed by "> Shall we?", ">" and "> Noon works."

### Requirement: Threading
A reply, reply-all or forward SHALL be filed into the original conversation in Gmail, and SHALL
carry In-Reply-To set to the answered message's Message-ID and References holding that
message's References followed by its Message-ID, without repeats, keeping the last 20. A new
message SHALL carry neither header.

#### Scenario: Reply threads in Gmail
- **WHEN** the user sends a reply
- **THEN** Gmail shows it in the same conversation as the message it answers

#### Scenario: Long reference chain
- **WHEN** the answered message already carries 25 References
- **THEN** the reply's References holds 20 ids, the last of which is the answered Message-ID

### Requirement: Outgoing message headers and body
Every sent message SHALL carry From set to the sending account's address, To, Cc and Subject
as composed, a Date, a fresh Message-ID ending in "@eloquawk.local" and a User-Agent of
"Eloquawk/" followed by the version. The message handed to Gmail SHALL include Bcc so that
Gmail delivers to Bcc recipients, and Gmail strips it before delivery. The body SHALL be plain
text in UTF-8; non-ASCII subjects and names SHALL arrive intact.

#### Scenario: Bcc delivered and hidden
- **WHEN** the user sends a message to ann@example.com with Bcc bob@example.com
- **THEN** both receive it and Ann's copy does not show Bob

#### Scenario: Non-ASCII subject
- **WHEN** the user sends a message with the subject "Café ☕ plans"
- **THEN** the recipient sees the subject "Café ☕ plans"

### Requirement: Sending and its outcome
ctrl-enter or Send SHALL send the message once; while it is sending, Send SHALL read
"Sending…" and Send and Attach… SHALL be disabled. On success the window SHALL close
and the main window SHALL show the notice "Message sent", and a reply SHALL appear in the
open conversation. On failure the window SHALL stay open with the error inline. The app
SHALL NOT retry a send on its own. When the outcome is unknown, the error SHALL say
"Couldn't confirm the message was sent. Check Sent before trying again." and Send SHALL stay
disabled until the user ticks "Send anyway".

#### Scenario: Reply appears in the thread
- **WHEN** the user sends a reply in demo mode
- **THEN** the compose window closes, the notice "Message sent" shows, and the open
  conversation has one more message

#### Scenario: Unknown outcome
- **WHEN** the network fails while a message is being sent
- **THEN** the compose window shows the unconfirmed-send error and Send is disabled
- **AND** ticking "Send anyway" enables Send again

#### Scenario: Gmail rejects the message
- **WHEN** Gmail answers the send with a client error
- **THEN** the compose window shows the error inline and Send stays enabled

### Requirement: Closing and quitting with unsent edits
Escape, ctrl-w and the window manager's close button SHALL close a compose window, first
asking "Discard this message?" with Discard and Keep when anything was edited; a second
Escape while asking SHALL keep the message. ctrl-q SHALL bring each edited compose window
forward in turn to ask, and quit once none is left. A window whose send is in flight SHALL
close without asking, and the main window SHALL report a failure of that send with the
notice "Couldn't send the message", or "The message may not have been sent" when the
outcome is unknown.

#### Scenario: Unedited window
- **WHEN** the user opens a new message and presses Escape
- **THEN** the window closes without asking

#### Scenario: Edited window
- **WHEN** the user types in the body and presses Escape
- **THEN** the window asks "Discard this message?"
- **AND** pressing Escape again keeps the window open with the text

#### Scenario: Quit with a draft open
- **WHEN** an edited compose window is open and the user presses ctrl-q
- **THEN** that window asks before the app quits

### Requirement: Mailto prefill
A `mailto:` link SHALL only ever open a new-message compose window; it SHALL never send. Only
the link's address and its `to`, `cc`, `bcc`, `subject` and `body` fields SHALL be honoured,
percent-decoded; every other field, including `attach`, `attachment`, `from`, `in-reply-to`
and `references`, SHALL be ignored. The prefill SHALL have no threading and no attachments
and SHALL be sent from the current account, or the open conversation's account for a link
in the reader. Links arrive from a click in the reader or as a launch argument, which a
running instance receives. A link that arrives with no signed-in account SHALL wait, with
the notice "Sign in to compose this message", and open once an account is ready.

#### Scenario: Whitelisted fields
- **WHEN** the user clicks `mailto:ann@example.com?cc=bob@example.com&subject=Lunch&body=Are%20you%20free%3F`
- **THEN** a "New message" window opens with To Ann, Cc Bob, subject "Lunch" and body
  "Are you free?"

#### Scenario: Dangerous fields ignored
- **WHEN** a link carries `attach=/etc/passwd`, `from=boss@example.com` and `in-reply-to=<x@y>`
- **THEN** the window has no attachment, From is the user's account and the message is not
  threaded

#### Scenario: Link from the desktop
- **WHEN** Eloquawk is running and another launch is given a `mailto:` URL
- **THEN** the running app opens a compose window for it and the new launch exits

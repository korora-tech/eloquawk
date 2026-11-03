# gmail-sync Specification

## Purpose
Keep each signed-in account's labels, counts and conversation lists in step with Gmail,
send the user's changes and messages, and recover from network trouble and rate limits
without losing work. Implementation detail lives in docs/SPEC.md sections 7.2 to 7.6 and
milestones M10 and M11. The encrypted snapshot shown at launch is the summary-cache
capability.

## Requirements

### Requirement: One request at a time, user actions first
The app SHALL make at most one Gmail request at a time. Opening a conversation, listing a
view, a change, a send and a save SHALL run before background sync work, and a long
background fetch SHALL be split into chunks of at most 10 conversations so a user action waits
for at most one chunk. The sidebar SHALL show "Syncing…" while a listing or a sync round runs
and "Synced <time>" after the last successful round.

#### Scenario: Opening a thread during a large first sync
- **WHEN** the user opens a conversation while the first sync is still fetching inbox rows
- **THEN** the conversation opens after at most one chunk of 10 rows, before the remaining rows load

### Requirement: First sync per account
After the keyring loads or an account signs in, the app SHALL read the account's change
cursor from the Gmail profile before listing anything, then fetch the labels and their
counts, then page 1 of the Inbox and of the view on screen. Rows SHALL appear as they load,
with grey rows for conversations still loading. The first sync SHALL NOT report new mail.

#### Scenario: Rows appear as they load
- **WHEN** an account's first sync lists the inbox
- **THEN** the list shows every listed conversation at once, as grey rows until each one's details arrive

#### Scenario: Failed first sync
- **WHEN** the first sync fails part-way
- **THEN** the next check runs a first sync again rather than a change check

### Requirement: Paging
Each view SHALL list the number of conversations set as the page size (50 by default) per
page, and scrolling near the end SHALL load the next page.

#### Scenario: Next page
- **WHEN** the user scrolls near the end of a view that has more conversations
- **THEN** the next page is appended below the current rows

### Requirement: Incremental change checks
Once an account has synced, the app SHALL ask Gmail's history for changes since the cursor
every 1, 5 or 15 minutes as set in Settings (1 by default), and on Ctrl+R, and SHALL refetch
only the conversations that changed, then page 1 of the Inbox and the view on screen and the
counts of the labels involved. The cursor SHALL advance only together with the changes it
covers, so a failure part-way through a round SHALL NOT lose a change. Conversations Gmail
no longer has SHALL leave the lists.

#### Scenario: A label change made in Gmail web
- **WHEN** the user archives a conversation in Gmail web and the next check runs
- **THEN** that conversation leaves the app's Inbox and the Inbox count updates

#### Scenario: Failure during a refetch
- **WHEN** the network drops while the changed conversations are being refetched
- **THEN** the conversations not yet refetched are fetched by a later round

#### Scenario: Manual refresh
- **WHEN** the user presses Ctrl+R
- **THEN** every account's backoff is cleared and its labels, page 1 and history are fetched again

### Requirement: Large bursts and expired history
When one round reports more than 100 changed conversations, the app SHALL refetch only the
newest new inbox mail (at most 20) and leave the rest to load when a listing shows them. When
Gmail no longer holds history back to the cursor, or a round runs past 100 history pages, the
account SHALL start over with a first sync, without reporting new mail for that round.

#### Scenario: History too old
- **WHEN** the app was offline long enough that Gmail rejects the history cursor
- **THEN** the account's lists and summary cache are emptied and a first sync runs
- **AND** no new-mail notification is shown for that round

### Requirement: New mail detection
A change check SHALL report a conversation as new mail when a message added to it carries
Inbox and Unread and none of Sent, Draft, Spam, Promotions, Social, Updates or Forums, and
when the refetched conversation is still unread and in the Inbox.

#### Scenario: Promotion arrives
- **WHEN** an unread inbox message filed under Promotions arrives
- **THEN** it is listed but not reported as new mail

#### Scenario: Read before the refetch
- **WHEN** a new inbox message is read in another client before the app refetches it
- **THEN** it is not reported as new mail

### Requirement: Changes apply at once and roll back on failure
Archive, trash, restore, star, read state and label changes SHALL show at once and be sent in
the background, one conversation at a time, without automatic retry beyond the single token
refresh. Starring SHALL star the conversation's last message that is not a draft; unstarring
SHALL clear the star from every message. Restore in the Trash view SHALL return the
conversation to the Inbox; undoing a trash SHALL put back the labels the conversation had.
After a change the app SHALL refetch the touched conversations and the
counts of the labels involved. If Gmail refuses a change, the app SHALL put the conversations
back as they were and show "Couldn't <action>: <reason>".

#### Scenario: Refused change
- **WHEN** the user archives a conversation and Gmail refuses the change
- **THEN** the conversation returns to its place in the Inbox with a "Couldn't archive: …" notice

#### Scenario: Change for a signed-out account
- **WHEN** a change is made for an account that is no longer signed in
- **THEN** it fails at once with "This account is not signed in."

### Requirement: Sending is never retried
A message SHALL be uploaded once and never retried. When the connection fails, Google answers
with a server error, or its success answer cannot be read, the composer SHALL say
"Couldn't confirm the message was sent. Check Sent before trying again." A refusal or a local
failure SHALL show its reason in the composer. Attachments over 25 MB in total SHALL be
refused before anything is uploaded. A sent message SHALL show "Message sent" and appear
through the next history round.

#### Scenario: Connection drops during send
- **WHEN** the network fails while a message is uploading
- **THEN** the composer shows the "Check Sent before trying again" message
- **AND** the app does not send it again

#### Scenario: Attachments too large
- **WHEN** the user sends a message whose attachments total more than 25 MB
- **THEN** nothing is uploaded and the composer says "Attachments are larger than 25 MB."

### Requirement: Backoff that defers rather than drops
After a network error, a rate limit, a server error or an unreadable answer, the account SHALL
back off from 5 seconds doubling up to 5 minutes, with jitter, and its background work SHALL
wait and then resume rather than be dropped; other accounts SHALL keep syncing. When Gmail
says a quota is used up, or a rate limit asks for more than 15 minutes, the account SHALL wait
until Google's stated time or 15 minutes, whichever is later, unless the user refreshes by
hand. The banner SHALL name the problem, the data on screen SHALL stay, and the problem SHALL
clear on the next success. A conversation Gmail refuses for good SHALL be reported without
stopping the rest of the sync.

#### Scenario: Offline for a while
- **WHEN** the network goes away and comes back
- **THEN** the banner shows "Couldn't reach Google. Check your connection; Ctrl+R tries again." while it is away
- **AND** the queued work resumes and the banner clears after the first success

#### Scenario: Rate limited
- **WHEN** Gmail answers a background fetch with a short rate limit
- **THEN** that chunk waits for the backoff and is then fetched, not lost

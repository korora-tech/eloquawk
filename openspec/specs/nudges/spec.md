# Nudges

## Purpose
Surface old mail that wants a second look, apart from the rest of the inbox, and let the user
dismiss it. The Gmail API has no nudge label or flag (Gmail's web app works nudges out on the
client), so this is a client-side approximation of Gmail's behaviour, not a copy of it.

## Requirements

### Requirement: Detection
A conversation SHALL be a nudge when it is 3 to 21 days old (by its newest message) and is
either unanswered received mail (newest message from someone else, still in the Inbox) or a
sent message nobody has answered (newest message from the account owner, to someone else).
Conversations in Promotions, Social, Updates or Forums, in Spam, Trash or Drafts, from
automated senders (`noreply`, `no-reply`, `donotreply`, `notifications`, `mailer-daemon`,
`bounce`), or whose newest sender is unknown SHALL never be nudges.

#### Scenario: Unanswered mail
- **WHEN** a primary-tab Inbox conversation from a person is 5 days old and nobody in it is the account owner on its newest message
- **THEN** it is a "Reply" nudge

#### Scenario: Unanswered sent mail
- **WHEN** the account owner sent the newest message 5 days ago to someone else
- **THEN** it is a "Follow up" nudge

### Requirement: Own section
The sidebar SHALL list "Nudges" right after Inbox with the count of nudges among loaded
conversations. The Nudges list SHALL show only nudges, and the Inbox list SHALL leave them
out, so nudges appear in one place.

#### Scenario: Old unanswered mail
- **WHEN** a 5-day-old unanswered conversation is in the Inbox
- **THEN** it is listed under Nudges and not in the Inbox

### Requirement: Dismissal
`d`, the row's Dismiss button and the palette's "Dismiss nudge" SHALL dismiss the targeted
nudges. A dismissed conversation SHALL stay out of Nudges (and return to the Inbox) until a
newer message arrives in it. Dismissals SHALL be kept per account in a file sealed with the
keyring's cache key, and deleted when the account signs out.

#### Scenario: Dismiss, then reply arrives
- **WHEN** the user dismisses a nudge and the other person later replies
- **THEN** the conversation is a nudge again once that reply is old enough

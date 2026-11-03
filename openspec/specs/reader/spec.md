# reader Specification

## Purpose
Show the open conversation as message cards with selectable text, safe links and an
action bar. Implementation detail lives in docs/SPEC.md section 8.2 and milestones M4–M5.
Message checks and label suggestions in the reader belong to their own capabilities.

## Requirements

### Requirement: Conversation as message cards
The reader SHALL show the open conversation's subject and user-label chips, then one card
per message. A collapsed card SHALL show the sender, snippet and date on one line; an
expanded card SHALL show the sender name and address, recipients, date, a Copy button,
the body and the attachments.

#### Scenario: Default expansion
- **WHEN** the user opens a conversation
- **THEN** unread messages and the latest message are expanded and the rest are collapsed

#### Scenario: New message in the open conversation
- **WHEN** a message arrives in the conversation the user has open
- **THEN** it gets its default expansion and the user's toggles on other cards are kept

#### Scenario: Expand and collapse all
- **WHEN** the user presses `;`
- **THEN** every card expands
- **AND** pressing `:` collapses every card

### Requirement: Message order
The reader SHALL list messages newest first by default, and SHALL offer a Settings
checkbox to list them oldest first. Card focus and `n`/`p` SHALL follow the order shown;
replies, forwards and star targets SHALL keep Gmail's order.

#### Scenario: Default order
- **WHEN** the user opens a conversation with several messages and the default settings
- **THEN** the latest message is the first card

#### Scenario: Oldest first
- **WHEN** the user turns off newest first in Settings and opens the same conversation
- **THEN** the oldest message is the first card

#### Scenario: Card navigation follows the screen
- **WHEN** the user presses `n` in a conversation shown newest first
- **THEN** focus moves to the next card down, which is the next older message

### Requirement: Selectable body text
Each expanded body SHALL be selectable by mouse drag and double-click, with ctrl-a and
ctrl-c in a focused body. A selection SHALL stay within one body. The Copy button SHALL
copy the visible body, including the quote when unfolded.

#### Scenario: Select all and copy
- **WHEN** the user focuses a body, presses ctrl-a, then ctrl-c
- **THEN** the clipboard holds that body's visible text with trailing whitespace trimmed

#### Scenario: Escape with a selection
- **WHEN** a body has a selection and the user presses Escape
- **THEN** the selection clears and the conversation stays open
- **AND** a second Escape closes the conversation

#### Scenario: Losing a focused body keeps the keys working
- **WHEN** the focused body goes away because the thread changes or its card collapses
- **THEN** focus moves to the reader and mail shortcuts keep working

### Requirement: Quoted text folding
A trailing block of `>`-quoted lines, or an "On … wrote:" line followed by quoted lines,
SHALL fold behind a toggle.

#### Scenario: Unfold a quote
- **WHEN** the user clicks the fold toggle
- **THEN** the quoted text appears below the body as its own selectable text

### Requirement: Only safe links open
The reader SHALL underline `http`, `https` and `mailto` links and bare `www.` hosts.
Clicking SHALL open web links in the browser and `mailto` links in the compose window
with a whitelisted prefill. No other scheme SHALL ever be linked or opened. An HTML
anchor whose text differs from its target SHALL show the target.

#### Scenario: Disguised link in an HTML message
- **WHEN** an HTML message has an anchor whose text differs from its href
- **THEN** the body shows the text followed by the real href

#### Scenario: Unsafe scheme
- **WHEN** a body contains `file:` or `javascript:` text
- **THEN** it is not underlined and cannot be opened

### Requirement: Bounded and private rendering
The reader SHALL NOT fetch remote images. It SHALL convert HTML bodies to text, show a
"Converted from HTML · Open in Gmail" caption, and cut bodies over 200 KB of text with a
"Message truncated · Open in Gmail" caption.

#### Scenario: Very large body
- **WHEN** the user opens a message whose text is over 200 KB
- **THEN** the body is cut and shows the truncation caption with an Open in Gmail link

### Requirement: Action bar
The reader SHALL offer Archive, Trash, Mark unread, Star, Labels…, Reply, Reply all,
Forward and Open in Gmail. In Trash, Trash SHALL be replaced by Restore; in Spam,
Archive SHALL be replaced by Not spam.

#### Scenario: Restore from Trash
- **WHEN** the user opens a conversation in the Trash view and clicks Restore
- **THEN** the conversation leaves Trash and returns to the Inbox

### Requirement: Attachments
Each attachment SHALL show as a row with its name, size and a Save button.

#### Scenario: Save in demo mode
- **WHEN** the user clicks Save in demo mode
- **THEN** the app shows the notice "Demo mode doesn't save files" and writes nothing

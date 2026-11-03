# keyboard-and-palette Specification

## Purpose
Let the user read and triage mail from the keyboard with Gmail-style keys, undo a change from
its notice, look up every key in an overlay, and run common commands from a searchable
palette. Implementation detail lives in docs/SPEC.md section 9 and milestone M6 with its
notes. The keys inside the reader's bodies, the compose window and AI label suggestions
belong to their own capabilities.

## Requirements

### Requirement: Mail keys never fire while typing
Mail keys SHALL work while the thread list, the reader or a message body has focus, and SHALL
NOT fire while any text field has focus, such as the search field, the label picker's filter,
the command palette's filter or a Settings field. Mail keys SHALL work only on the mail
screen.

#### Scenario: Typing in search
- **WHEN** the search field has focus and the user types `e`
- **THEN** the field holds "e" and no conversation is archived

#### Scenario: Keys with a body focused
- **WHEN** the user has clicked into a message body and presses `j`
- **THEN** the cursor moves to the next conversation

### Requirement: Navigation keys
The app SHALL bind these navigation keys:

| Key | Behaviour |
|---|---|
| `j` / down, `k` / up | Move the cursor down or up; when the reader is open, open the conversation under it |
| `o` / enter | Open the cursor conversation; when the reader already shows it and a card is focused, toggle that card |
| `u` / escape | Back out one layer (see the escape requirement) |
| `n` / `p` | Focus the next or previous message card, expand it and scroll it into view; with none focused, start at the first or last card |
| space / page down, shift-space / page up | Scroll the reader by a page |
| `;` / `:` | Expand or collapse every card |
| `g i`, `g s`, `g t`, `g d`, `g a` | Show the current account's Inbox, Starred, Sent, Drafts or All Mail |
| `g n` / `g p` | Show the next or previous account's Inbox, wrapping round |

#### Scenario: Reading on with j
- **WHEN** the reader shows the conversation on row 2 and the user presses `j`
- **THEN** the cursor is on row 3 and the reader shows that conversation

#### Scenario: Go to Sent
- **WHEN** the user presses `g` then `t`
- **THEN** the list shows the current account's Sent view

#### Scenario: Next account wraps
- **WHEN** the last account's view is shown and the user presses `g` then `n`
- **THEN** the first account's Inbox is shown

### Requirement: Triage keys
The app SHALL bind these triage keys, each but `z` acting on the selection when it is not empty, else
on the open conversation, else on the cursor row:

| Key | Behaviour |
|---|---|
| `e` | Archive; in the Spam view, Not spam |
| `#` | Move to Trash; in the Trash view, Restore to the Inbox |
| `!` | Report spam; in the Spam view, Not spam |
| `s` | Star, or unstar when every target is already starred |
| shift-i / shift-u | Mark as read or unread |
| `l` | Open the label picker |
| `z` | Undo the last undoable change while its notice shows |

#### Scenario: Restore from Trash with the key
- **WHEN** the user presses `#` on a conversation in the Trash view
- **THEN** it leaves Trash and returns to the Inbox

#### Scenario: Not spam from the Spam view
- **WHEN** the user presses `e` on a conversation in the Spam view
- **THEN** it leaves Spam and appears in the Inbox

#### Scenario: Star a mixed selection
- **WHEN** the user selects one starred and one unstarred conversation and presses `s`
- **THEN** both are starred

### Requirement: Selection and writing keys
`x` SHALL toggle the cursor conversation in the selection. `c` SHALL open a new message from
the current account, or show "Add an account to write mail" without one. `r`, `a` and `f`
SHALL reply, reply all or forward to the open conversation, else the cursor row, opening it
first when needed; with neither they SHALL show "No conversation to reply to" or "No
conversation to forward".

#### Scenario: Select with x
- **WHEN** the user presses `x`, `j`, `x`
- **THEN** the first two rows are selected and the title row shows "2 selected"

#### Scenario: Reply from the list
- **WHEN** no conversation is open and the user presses `r`
- **THEN** the cursor conversation opens and a reply window for it follows

### Requirement: Escape backs out one layer
Escape and `u` SHALL undo one layer per press: close the help overlay when it shows, else
close the reader, else clear the selection, and leave the focus on the list. Escape in a
message body with a text selection SHALL clear that selection first, as the reader specifies. Escape SHALL
close the label picker or the command palette from their filters, clear the search field
from inside it, and close Settings on the Settings screen.

#### Scenario: Reader then selection
- **WHEN** a conversation is open with two others selected and the user presses escape twice
- **THEN** the first press closes the reader with the selection kept
- **AND** the second press clears the selection

### Requirement: App-wide keys
These keys SHALL work anywhere in the main window, text fields included: ctrl-k opens the
command palette, ctrl-r refreshes, ctrl-, opens Settings, ctrl-n opens a new message, ctrl-w
closes the window and ctrl-q quits. ctrl-b SHALL fold or unfold the sidebar as a mail key.

#### Scenario: Refresh while typing
- **WHEN** the search field has focus and the user presses ctrl-r
- **THEN** the app syncs and the field keeps its text

### Requirement: Notices with Undo
Archive, Trash, Restore, Report spam, Not spam and label changes SHALL post a notice at the
bottom right naming what changed with an Undo button, such as "Archived 1 conversation" or
"Moved 2 conversations to Trash". Read, unread and star changes SHALL post no notice. A notice
SHALL disappear after 5 seconds. Undo, by button or `z`, SHALL reverse only the last undoable
change, put its conversations back where they were with the labels they had, and show
"Action undone"; once its notice has gone or another notice has replaced it, nothing SHALL be
undoable.

#### Scenario: Undo an archive
- **WHEN** the user archives a conversation and presses `z` within 5 seconds
- **THEN** it is back in the Inbox at its old row and the notice reads "Action undone"

#### Scenario: Undo a trash
- **WHEN** the user undoes moving a starred, labelled inbox conversation to Trash
- **THEN** it is back in the Inbox with its star and labels

#### Scenario: Too late to undo
- **WHEN** the archive notice has disappeared and the user presses `z`
- **THEN** nothing changes

### Requirement: Shortcut overlay
`?` SHALL show an overlay titled "Keyboard shortcuts" that lists the key bindings grouped by
where it works (Mail, Message text, Label picker, Search, Compose window, Settings, Anywhere),
showing keys as, for example, "Shift I" and "g then i". It SHALL close on `?`, escape, `u` or a
click outside its card, with the focus left on the list. While it shows, every other mail key
SHALL do nothing, and clicks, hover and scrolling SHALL NOT reach the panes beneath it.

#### Scenario: Keys are blocked under the overlay
- **WHEN** the overlay shows and the user presses `e`
- **THEN** no conversation is archived and the overlay stays open

#### Scenario: Click outside
- **WHEN** the user clicks the dimmed area over a thread row
- **THEN** the overlay closes and the row does not open

### Requirement: Command palette
ctrl-k or the header's "Commands (Ctrl K)" button SHALL open a palette titled "Commands" over
the mail screen, closing the shortcut overlay and the label picker, with its "Search commands"
filter focused. The palette SHALL list each command with its key, filtered by case-insensitive
substring of its name, and show "No matching commands" when none match. Up and down SHALL move
the highlight, which typing resets to the first row; enter or a click SHALL close the palette
and run the command exactly as its key would. Escape or a click outside SHALL close it and
return focus to the list. The palette SHALL NOT open outside the mail screen.

The commands SHALL be: Compose, Search mail, Go to Inbox, Go to Starred, Go to Sent, Go to
Drafts, Go to All Mail, Archive conversation, Trash conversation, Star or unstar, Mark as read,
Mark as unread, Label conversation, Reply, Reply all, Forward, Refresh, Settings, Fold or
unfold the sidebar, and Keyboard shortcuts.

#### Scenario: Run a command by name
- **WHEN** the user presses ctrl-k, types "inbox" and presses enter
- **THEN** the palette closes and the current account's Inbox is shown

#### Scenario: Archive from the palette
- **WHEN** a conversation is open and the user runs "Archive conversation" from the palette
- **THEN** it is archived, the next row opens and the Undo notice shows

#### Scenario: Not on the Settings screen
- **WHEN** Settings is open and the user presses ctrl-k
- **THEN** no palette opens

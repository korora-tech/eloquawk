# thread-list Specification

## Purpose
Show each account's labels in the sidebar and the current view's conversations in the thread
list, with a cursor, a selection, paging and changes that show at once. Implementation detail
lives in docs/SPEC.md sections 8.1 and 8.2. The reader, search, the label picker and AI badges
on rows belong to their own capabilities.

## Requirements

### Requirement: Sidebar account sections
The sidebar SHALL show a Compose button, then one section per signed-in account headed by its
address, then an account box with the sign-in and sync state. Each section SHALL list Inbox,
Starred, Important, Sent, Drafts, All Mail, Spam and Trash, then the account's visible user
labels sorted by name and nested under their parent on `/`, each with its colour dot. Hidden
user labels SHALL NOT appear in the sidebar. Inbox, Spam and user labels SHALL show their
unread conversation count, Drafts its total, and no label SHALL show a count of zero.

#### Scenario: Nested labels
- **WHEN** an account has user labels "Projects" and "Projects/Eloquawk"
- **THEN** "Eloquawk" is listed indented under "Projects"

#### Scenario: Counts follow a change
- **WHEN** the user archives an unread inbox conversation
- **THEN** the Inbox count drops by one straight away

### Requirement: Collapsible sections and label trees
Clicking an account header SHALL collapse or expand that account's section, and a collapsed
header SHALL show the account's inbox unread count. A user label with child labels SHALL have
a toggle that hides or shows its children. Both choices SHALL be remembered across launches.

#### Scenario: Collapse an account
- **WHEN** the user clicks the header of an account with 3 unread inbox conversations
- **THEN** its labels are hidden and the header shows 3
- **AND** the section stays collapsed after a relaunch

#### Scenario: Collapse a label tree
- **WHEN** the user clicks the toggle beside "Projects"
- **THEN** "Projects/Eloquawk" is hidden and "Projects" stays listed

### Requirement: Sidebar folds to a rail
The sidebar SHALL fold to a narrow rail of icons, from a button beside Compose, ctrl-b or the
command palette, and unfold the same ways. The rail SHALL hold an unfold button, Compose, and
Inbox, Starred, Sent, Drafts and All Mail of the current account, with the inbox's unread
count on the Inbox icon and the current view highlighted. The folded state SHALL be
remembered across launches.

#### Scenario: Fold and use the rail
- **WHEN** the user presses ctrl-b and clicks the Starred icon
- **THEN** the sidebar is a rail of icons and the list shows the current account's Starred view

#### Scenario: Folded on relaunch
- **WHEN** the user folds the sidebar and relaunches the app
- **THEN** the sidebar starts as the rail

### Requirement: Choosing a view
Clicking a sidebar label SHALL show that view in the thread list, ask for its first page, put
the cursor on the first row, clear the selection and close the reader. The app SHALL remember
the last label or All Mail view shown and show it again at launch, falling back to the first
account's Inbox when that account is gone. Searches SHALL NOT be remembered as the last view.

#### Scenario: Restore the last view
- **WHEN** the user shows a user label, quits and relaunches
- **THEN** the same label view is shown

#### Scenario: Switching view drops the selection
- **WHEN** the user has two conversations selected and clicks another label
- **THEN** the new view shows with nothing selected and the reader closed

### Requirement: List title row
Above the rows the list SHALL show the view's name, using a nested label's own name, and the
number of conversations: the label's total for a label view, otherwise the rows loaded. While
conversations are selected it SHALL show "N selected" and a Clear button that empties the
selection.

#### Scenario: Selection count
- **WHEN** the user selects two conversations
- **THEN** the title row shows "2 selected" and a Clear button
- **AND** clicking Clear empties the selection

### Requirement: Thread rows
Each row SHALL show a selection checkbox, an unread dot, the participants, the date, the
subject (or "(no subject)") followed by " — " and the snippet, an attachment mark when the
conversation has attachments, and a star. Unread rows SHALL set the participants and subject
in bold. A row whose summary is still missing SHALL show as a grey skeleton. An empty view
SHALL show "Loading…" while its first page loads, otherwise "No conversations".

#### Scenario: Read and unread rows
- **WHEN** the list holds one unread and one read conversation
- **THEN** only the unread row has the dot and bold participants and subject

#### Scenario: Empty label
- **WHEN** the user shows a label with no conversations and its listing has answered
- **THEN** the list shows "No conversations"

### Requirement: Paging
The list SHALL load further pages on demand. When the cursor or the visible rows come within
10 rows of the end and more conversations exist, the next page SHALL be requested once. Below
the rows the list SHALL show "Loading…" while a page loads, otherwise a "Load more" button
while more exist. A conversation SHALL NOT be listed twice.

#### Scenario: Scrolling near the end
- **WHEN** the user moves the cursor into the last 10 loaded rows of a view with more pages
- **THEN** the next page is requested and its rows are appended without duplicates

#### Scenario: Load more
- **WHEN** the user clicks "Load more"
- **THEN** the footer shows "Loading…" until the next page arrives

### Requirement: Cursor and opening
The list SHALL keep a cursor row, marked with the selected background and an accent left
border, and SHALL scroll to keep it in view however it moves. Moving the cursor SHALL stop at
the first and last rows. The cursor SHALL stay on the same conversation when pages or updates
reorder the list; when its conversation leaves the list, it SHALL move to the row now at the
same position, clamped to the list. Clicking a row SHALL move the cursor there and open the
conversation in the reader.

#### Scenario: Cursor clamps
- **WHEN** the cursor is on the last row and the user moves it down
- **THEN** the cursor stays on the last row

#### Scenario: Refresh keeps the cursor
- **WHEN** a refresh brings new conversations in above the cursor
- **THEN** the cursor is still on the same conversation

#### Scenario: Click a row
- **WHEN** the user clicks the third row
- **THEN** the cursor is on the third row and that conversation opens in the reader

### Requirement: Selection
Clicking a row's checkbox SHALL toggle that conversation in the selection without opening it,
and a selected row SHALL show a filled checkbox. While the selection is not empty, triage
actions SHALL act on every selected conversation. A selected conversation that leaves the
list SHALL leave the selection.

#### Scenario: Bulk archive
- **WHEN** the user selects two inbox conversations and archives
- **THEN** both leave the Inbox and the selection is empty

### Requirement: Changes show at once
An archive, trash, spam, label, read or star change SHALL show in the list and the counts
before the server answers. A conversation SHALL leave every loaded view it no longer belongs
to: a label view when the label is removed, and every view but Trash or Spam when it moves
there. A conversation that gains a label SHALL join that label's loaded view in date order.
If the server refuses, the conversations SHALL return to their places and the app SHALL show
"Couldn't <verb>: <error>", for example "Couldn't archive: …".

#### Scenario: Trash leaves All Mail
- **WHEN** the user moves a conversation to Trash
- **THEN** it leaves the Inbox and All Mail lists at once

#### Scenario: Refused change
- **WHEN** the server refuses an archive
- **THEN** the conversation is back at its row and the notice starts "Couldn't archive:"

### Requirement: Auto-advance
When the open conversation leaves the current list because of a change, the reader SHALL open
the next row below it, else the row above it, else close; the cursor SHALL follow the opened
row.

#### Scenario: Archive the open conversation
- **WHEN** the user archives the open conversation on row 2 of the Inbox
- **THEN** the conversation that was on row 3 opens and the cursor is on it

#### Scenario: Last conversation
- **WHEN** the user archives the only conversation in the list while it is open
- **THEN** the reader closes

### Requirement: Unread and star state
Opening an unread conversation SHALL mark it read when the "Mark as read when opened" setting is on,
which it is by default, with no notice and no undo. Clicking a row's star SHALL star or unstar
that conversation only, whatever the selection holds, with ★ for starred and ☆ for not.

#### Scenario: Opening marks read
- **WHEN** the user opens an unread conversation with the default settings
- **THEN** its row loses the unread styling and the Inbox count drops by one

#### Scenario: Star from the row
- **WHEN** the user has another conversation selected and clicks the star on an unstarred row
- **THEN** only that row's conversation becomes starred

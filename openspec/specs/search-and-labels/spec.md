# search-and-labels Specification

## Purpose
Find conversations with Gmail's search syntax, and file them with user labels from a
keyboard-driven picker and the sidebar's label views. Implementation detail lives in
docs/SPEC.md section 9 ("Search behaviour") and milestone M7 with its notes. The sidebar
layout belongs to thread-list, and AI label suggestions to their own capability.

## Requirements

### Requirement: Search field
The header SHALL hold a "Search mail" field. `/` SHALL focus it. Enter SHALL search the
current account, show the results in the thread list and focus the list. While results show,
the header SHALL show "Search results for “q” ✕" and the list title SHALL read the same.
Typing in the field SHALL NOT trigger mail shortcuts.

#### Scenario: Run a search
- **WHEN** the user presses `/`, types `from:zoë` and presses Enter
- **THEN** the list shows the current account's matching conversations
- **AND** the header shows "Search results for “from:zoë” ✕"

#### Scenario: Typing is not a shortcut
- **WHEN** the user presses `/` then `j`
- **THEN** the field holds "j" and the cursor has not moved

### Requirement: Queries go to Gmail as written
The app SHALL trim the query and send it to Gmail unchanged, so Gmail's own syntax is the
contract. A query with no terms, such as whitespace or empty quotes, SHALL leave any search
being shown instead of searching.

#### Scenario: Empty query
- **WHEN** results are shown and the user clears the field and presses Enter
- **THEN** the list returns to the view the search replaced

### Requirement: Leaving a search
Clicking ✕ SHALL return to the view the search replaced, or to the account's Inbox when that
view is unknown or belonged to another account. Escape in the field SHALL clear it and focus
the list, leaving the results shown. While results show, the field SHALL hold their query,
and it SHALL be cleared once the list leaves the search.

#### Scenario: Close the results
- **WHEN** the user searches from the Starred view and clicks ✕
- **THEN** the Starred view is shown and the field is empty

#### Scenario: Escape keeps the results
- **WHEN** the user presses Escape in the search field
- **THEN** the field is empty, the list has focus and the results are still shown

### Requirement: Demo mode matches a subset locally
In demo mode the app SHALL match queries itself, supporting case-insensitive free terms
(all must match), quoted phrases, `-term` negation, `OR` between terms, `from:`, `to:`, `cc:`,
`subject:`, `label:`, `in:inbox|sent|trash|spam|anywhere|starred`,
`is:unread|read|starred|important`, `has:attachment`, `newer_than:` and `older_than:` with
`d`, `m` or `y`, and `after:` and `before:` as `YYYY/MM/DD`. An unknown operator SHALL match
as free text, and no query SHALL fail to parse. Conversations in Spam or Trash SHALL match
only when the query asks for them with `in:spam`, `in:trash`, `in:anywhere` or the matching
`label:`.

#### Scenario: Negation and OR
- **WHEN** the user searches `invoice OR receipt -paid` in demo mode
- **THEN** the results hold conversations mentioning invoice or receipt, none mentioning paid

#### Scenario: Trash left out
- **WHEN** the user searches a word that appears only in a trashed conversation
- **THEN** there are no results
- **AND** searching the same word with `in:trash` finds it

#### Scenario: Unknown operator
- **WHEN** the user searches `filename:report`
- **THEN** the query matches the text "filename:report" and no error is shown

### Requirement: Changes to search results
Moving a result to Trash or Spam SHALL take it out of the results unless the query asks for
Trash or Spam. Other label changes SHALL leave the results as they are until the search runs
again. Results SHALL never be kept between launches.

#### Scenario: Archive a result
- **WHEN** the user archives a conversation shown in search results
- **THEN** it stays in the results

#### Scenario: Trash a result
- **WHEN** the user moves a result to Trash in a search without `in:trash`
- **THEN** it leaves the results

### Requirement: Label views
Clicking a user label in the sidebar SHALL list the account's conversations with that label,
titled with the label's own name. Adding a label to a conversation SHALL bring it into that
label's loaded view in date order, and removing it SHALL take it out, both at once, with the
label's unread count following.

#### Scenario: Remove the label being viewed
- **WHEN** the user removes "Travel" from a conversation while viewing "Travel"
- **THEN** the conversation leaves the list and the next row opens if it was open

### Requirement: Opening the label picker
`l` SHALL open the label picker for the selection, else the open conversation, else the cursor
row; the reader's "Labels…" button SHALL open it for the open conversation only. With nothing
to act on, `l` SHALL show the notice "No conversation to label". The picker SHALL be titled
"Label conversation" or "Label N conversations" and SHALL act on the conversations it opened
for until it closes.

#### Scenario: Picker for a selection
- **WHEN** the user selects three conversations and presses `l`
- **THEN** the picker opens titled "Label 3 conversations" with its filter focused

#### Scenario: Nothing to label
- **WHEN** the list is empty and the user presses `l`
- **THEN** the notice "No conversation to label" shows and no picker opens

### Requirement: Picker rows and filter
The picker SHALL list every user label of the account, hidden ones included, sorted by full
name, and SHALL NOT list system labels. Each row SHALL show ✓ when every target has the label
and – when only some do. Typing SHALL filter the rows by case-insensitive substring and move
the highlight to the first row; up and down SHALL move the highlight and keep it in view.
With no labels it SHALL show "No labels", and with no match "No matching labels". Typing in
the filter SHALL NOT trigger mail shortcuts.

#### Scenario: Filter and highlight
- **WHEN** the user types "tra" in the filter
- **THEN** only labels containing "tra" in any case are listed, the first highlighted

#### Scenario: Typing e does not archive
- **WHEN** the user types `e` in the filter
- **THEN** the rows are filtered and no conversation is archived

### Requirement: Applying and removing labels
Enter or a click on a row SHALL add the label to every target unless all of them already have
it, in which case it SHALL remove it from all of them. The change SHALL show at once, post an
undoable notice such as "Added “Travel” to 1 conversation" or "Removed “Travel” from 2
conversations", and leave the picker open for more toggles. Escape or a click outside SHALL
close the picker and return focus to the list.

#### Scenario: Add then remove
- **WHEN** the user presses Enter on "Travel" for a conversation without it
- **THEN** the row shows ✓ and the notice reads "Added “Travel” to 1 conversation"
- **AND** pressing Enter again removes it with "Removed “Travel” from 1 conversation"

#### Scenario: Partly labelled selection
- **WHEN** two selected conversations are toggled on a label only one of them has
- **THEN** both have the label

#### Scenario: Undo a label change
- **WHEN** the user adds a label, closes the picker and presses `z` while the notice shows
- **THEN** the label is removed again

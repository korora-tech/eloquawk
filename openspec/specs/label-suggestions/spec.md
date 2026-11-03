# label-suggestions Specification

## Purpose
Offer up to a few of the account's own labels as one-key next actions when the user opens a
message, using the same Jev check as message checks, without ever filing mail by itself.
Implementation detail lives in docs/SPEC.md section 16.8, the label parts of 16.2, 16.6,
16.10 and 16.12.3, milestone M17, and the "Suggested labels" section of docs/ai-triage.md.

## Requirements

### Requirement: Suggest labels toggle
Settings SHALL offer "Suggest labels when I open a message", on by default, captioned "Your
label names are sent with each check of a message you open while this is on. Up to 100
labels are considered (fewer for very long messages)." Label questions SHALL be asked only
for accounts opted in to message checks and only for a message the user opens; background
checks SHALL never carry label names. Turning the toggle off SHALL stop label names being
sent and hide every suggestion at once; turning it on SHALL ask about the open message
straight away.

#### Scenario: Toggle off
- **WHEN** the user turns "Suggest labels when I open a message" off and opens a message
- **THEN** the check carries no label names and no suggestion row shows

#### Scenario: Background check
- **WHEN** background checks are on and new mail arrives
- **THEN** the background check carries no label names

### Requirement: Which label names are disclosed
Each label check SHALL send the names of the account's user labels only, never system,
category or hidden labels, cut to 100 characters, up to 100 labels: the children of the
required label group first, then by how many conversations they hold. Label ids SHALL NOT
be sent; each label SHALL be asked as a separate yes/no question and mapped back on the
machine. Every candidate's name SHALL be sent, with no per-label exclusion. The disclosure
SHALL say that, while label suggestions are on, the names of the user's labels (up to 100)
are sent with each check of a message the user opens.

#### Scenario: Hidden and system labels
- **WHEN** an account has a hidden user label and the usual system labels
- **THEN** neither the hidden label nor any system or category label is sent

#### Scenario: Many labels
- **WHEN** an account has 150 user labels
- **THEN** at most 100 names are sent with a check

### Requirement: Label guidance
Settings SHALL offer a "LABEL GUIDANCE" field, up to 500 characters, saved on the device with
Save guidance and captioned "Saved on this device and sent to OpenRouter and TypeSafe with
label suggestions. Avoid secrets and private information." The guidance SHALL be sent, with
control characters stripped and without redaction, only with label questions, never with a
check that asks no labels. Changing it SHALL ask about the open message's labels again.

#### Scenario: Guidance only with labels
- **WHEN** guidance is saved and suggestions are turned off
- **THEN** checks carry neither label names nor the guidance

### Requirement: Choosing suggestions
The app SHALL suggest every label scoring above 0.60, highest first and then by name. When
none scores above 0.60, it SHALL offer every label tied for the highest score, marked
"(best guess)". When a label and one of its nested labels both qualify, only the nested one
SHALL be shown. Labels already on the conversation SHALL NOT be suggested, so an applied
label disappears from the row without a new request.

#### Scenario: Order receipt
- **WHEN** the user opens "Your order receipt #4471" and "Receipts" scores 0.89
- **THEN** the row offers "Receipts"

#### Scenario: Nested label wins
- **WHEN** both "Projects" and "Projects/Eloquawk" score above 0.60
- **THEN** only "Projects › Eloquawk" is offered

#### Scenario: Already labelled
- **WHEN** the user opens a receipt that already carries "Receipts"
- **THEN** "Receipts" is not offered

### Requirement: Suggestion chips under the subject
The reader SHALL show the suggestions below the subject and any banner, after the muted
caption "Suggested labels", one outlined chip each with its key hint (`1` to `3`, on the
first three chips only), the label's colour dot when it has one, and its name with nesting
shown as " › ". A ✕ SHALL hide the row for that message for the rest of the session.

#### Scenario: Dismiss the row
- **WHEN** the user clicks ✕ on the suggestion row
- **THEN** the row stays hidden for that message until the app restarts

### Requirement: Applying a suggestion
Clicking a chip, or pressing `1`, `2` or `3` in the mail context, SHALL add that label to the
open conversation only, never to the x-selection, through the label picker's undoable path,
with the toast "Added “<name>” to 1 conversation" and Undo; `z` SHALL undo it while the
toast shows. A key with no matching suggestion, or with no conversation open, SHALL do
nothing and post no notice. The keys SHALL do nothing while a text field has focus or the
help overlay shows, and the help overlay and README SHALL list them as "Apply suggested
label 1" to "3".

#### Scenario: Apply with a key
- **WHEN** the user opens "Your order receipt #4471" and presses `1`
- **THEN** "Receipts" appears as a label chip under the subject, the suggestion disappears,
  and the toast reads "Added “Receipts” to 1 conversation" with Undo
- **AND** `z` removes the label again

#### Scenario: Digit in the search field
- **WHEN** the user types `1` in the search field
- **THEN** "1" is inserted and no label is applied

### Requirement: When suggestions are hidden
The app SHALL show no suggestions, and `1`, `2` and `3` SHALL do nothing, while the
message's check is in flight, while suggestions are off, for an account not opted in, after
the row was dismissed, while the stored label answers predate a change to the account's
labels, guidance or label group, and for risky mail: a warning or danger verdict or a local
caution. The one exception for risky mail is the required label group's suggestion.

#### Scenario: Check in flight
- **WHEN** a message's check is still running
- **THEN** no suggestion row shows and `1` does nothing

#### Scenario: Risky mail
- **WHEN** the user opens a message showing the local caution whose text mentions "Receipts"
- **THEN** no suggestion row shows

### Requirement: Required label group
Settings SHALL offer "ALWAYS SUGGEST FROM LABEL GROUP", a parent label name saved with Save
label group and blank by default. When set, the app SHALL offer the best-scoring child of
that group whatever its score, including on risky mail, at the front of the row when it is
not already offered, unless a child of the group is already on the conversation. The group's
own parent label SHALL NOT be offered while the group is set. A child scoring 0.60 or less SHALL be marked
"(best guess)". The suggestion SHALL still need a successful check and SHALL never be
applied automatically.

#### Scenario: Low-scoring group child
- **WHEN** the group is "TTL" and its best child "TTL/M3" scores 0.30
- **THEN** the row offers "TTL › M3 (best guess)"

#### Scenario: Group already satisfied
- **WHEN** the conversation already carries a child of the group
- **THEN** no group suggestion is added

### Requirement: Asking labels again
A change to the account's labels (a rename, a new or deleted label, hiding one), to the
guidance or to the label group SHALL make the next check of an already checked message ask
the label questions only, not the phishing, scam and spam questions. Changes to label counts
and applying a suggestion SHALL NOT ask again. When a labels-only ask fails, the stored
verdict SHALL stand with no "Couldn't check" caption, and the labels SHALL be asked again on
the next open. When a background check answers without labels for the conversation the user
has open, the app SHALL ask its labels at once.

#### Scenario: Label rename
- **WHEN** the user renames a label and reopens a message checked before
- **THEN** one request is sent and it asks no phishing question

#### Scenario: Failed labels-only ask
- **WHEN** a labels-only ask fails for a message with a clean verdict
- **THEN** the reader shows no failure caption

### Requirement: Long messages and many labels
A check SHALL keep every request within the size budget without cutting the phishing, scam
and spam input for labels: labels that do not fit SHALL move to a second labels-only request
with the same redacted extract, and a check SHALL make at most two requests. Labels that fit
in neither SHALL be skipped for that message, and even the worst case SHALL ask at least 30
labels.
If the second request fails or is skipped, the labels it carried SHALL be asked again on the
next open.

#### Scenario: Typical message with 100 labels
- **WHEN** a 6,000-character message is checked on an account with 100 labels of 20
  characters
- **THEN** two requests are sent, the body is not cut and all 100 labels are asked

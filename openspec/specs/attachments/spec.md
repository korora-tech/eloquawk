# attachments Specification

## Purpose
List a message's attachments, save them to the user's download folder without ever
overwriting a file, and send files and forwarded attachments within Gmail's size limit.
Implementation detail lives in docs/SPEC.md sections 7.5 (`Send`, `SaveAttachment`) and 8.5
and milestones M8 and M12. A saved attachment is the only mail the app writes to disk.

## Requirements

### Requirement: Listing attachments
The reader SHALL show each attachment of an expanded message as a row with its file name,
its human-readable size and a Save button. An attachment without a name SHALL be listed as
"attachment". A thread row SHALL show a paper-clip mark when the thread has attachments.

#### Scenario: Message with a PDF
- **WHEN** the user opens a conversation whose message carries receipt-10442.pdf
- **THEN** the expanded card shows a row with "receipt-10442.pdf", its size and Save

### Requirement: Saving to the download folder
Save SHALL fetch the attachment afresh from Gmail and write it to the folder named by
`XDG_DOWNLOAD_DIR` in the user's `user-dirs.dirs`, where only a leading `$HOME` is expanded
and the result must be an absolute path; otherwise it SHALL use `~/Downloads`. The folder
SHALL be created when missing.

#### Scenario: Custom download folder
- **WHEN** `user-dirs.dirs` sets `XDG_DOWNLOAD_DIR="$HOME/Incoming"` and the user saves an
  attachment
- **THEN** the file is written to `~/Incoming`

#### Scenario: Unusable setting
- **WHEN** `XDG_DOWNLOAD_DIR` is a relative path or uses another variable
- **THEN** the file is written to `~/Downloads`

### Requirement: Safe file names
The saved file's name SHALL come from the attachment's name with path separators, NUL,
control characters and bidi or invisible formatting characters removed, leading dots and
spaces and trailing whitespace trimmed, and the result cut to 200 bytes while keeping the
extension. A name that ends up empty, `.` or `..` SHALL become "attachment". A save SHALL
never write outside the download folder.

#### Scenario: Path traversal
- **WHEN** the user saves an attachment named `../../etc/passwd`
- **THEN** it is saved in the download folder as `etcpasswd`

#### Scenario: Disguised extension
- **WHEN** the attachment name uses a right-to-left override to look like a PDF
- **THEN** the saved name has the override removed and shows its real extension

#### Scenario: Hidden file
- **WHEN** the attachment is named `.bashrc`
- **THEN** it is saved as `bashrc`

### Requirement: Never overwrite
A save SHALL NOT replace an existing file. When the name is taken it SHALL try
`name (1).ext`, `name (2).ext` and so on. A write that fails part-way SHALL remove the
partial file.

#### Scenario: Saving twice
- **WHEN** the user saves a.pdf twice into an empty download folder
- **THEN** the folder holds `a.pdf` and `a (1).pdf`

### Requirement: Save outcome
A successful save SHALL show the notice "Saved <file> to <folder>", with the home folder
shown as `~`, and an "Open folder" action that reveals the folder. A failed save SHALL show
"Couldn't save <file>: <reason>", where the reason is "Attachment too large to download",
"The attachment is no longer in Gmail.", "Gmail sent the attachment's data in a form it
couldn't read.", "This account is not signed in.", a short file-system reason such as
"permission denied" or "the disk is full", or the usual message for a network or Gmail
error.

#### Scenario: Saved
- **WHEN** the user saves report.pdf and the download folder is `~/Downloads`
- **THEN** the notice reads "Saved report.pdf to ~/Downloads" with "Open folder"

#### Scenario: Attachment removed from Gmail
- **WHEN** the message's attachment part no longer exists in Gmail
- **THEN** the notice reads "Couldn't save <file>: The attachment is no longer in Gmail."

### Requirement: Attaching files in the composer
The compose window SHALL offer "Attach…", which opens the desktop file chooser for one or
more files, and SHALL show each attachment as a chip with its name, size and a ✕ that
removes it. Folders SHALL be refused with "Only files can be attached", an unreadable file
with "Couldn't read that file", and a missing file chooser with "Couldn't open the file
chooser", leaving the button usable. Each local file SHALL be sent with a type guessed from
its extension, or `application/octet-stream`.

#### Scenario: Remove a chip
- **WHEN** the user attaches two files and clicks ✕ on the first
- **THEN** one chip remains and the message is sent with that file only

#### Scenario: No file chooser
- **WHEN** the desktop has no file chooser portal and the user clicks "Attach…"
- **THEN** the window shows "Couldn't open the file chooser" and "Attach…" stays enabled

### Requirement: 25 MB budget
The attachments of one message, local and forwarded, SHALL total at most 25 MB. A file that
would take the total past it SHALL be refused when attached with "Attachments can't exceed
25 MB in total". At send time the bytes actually read SHALL be checked again, and a message
over the limit SHALL fail with "Attachments are larger than 25 MB." and nothing SHALL be
uploaded.

#### Scenario: Too much attached
- **WHEN** the user has attached 20 MB and attaches a 10 MB file
- **THEN** the 10 MB file is not added and the window shows "Attachments can't exceed 25 MB in
  total"

#### Scenario: File grew after attaching
- **WHEN** an attached file grows past the budget before the user presses Send
- **THEN** the send fails with "Attachments are larger than 25 MB." and nothing is sent

### Requirement: Forwarding attachments
A forward SHALL start with the original message's attachments as removable chips. They SHALL
be fetched from Gmail when the message is sent, in the order the chips show. If one cannot be
fetched, the send SHALL fail in the compose window with "Couldn't attach <file>: <reason>"
and nothing SHALL be sent.

#### Scenario: Forward with an attachment
- **WHEN** the user forwards a message carrying a.pdf and sends it
- **THEN** the forwarded message carries a.pdf with the original bytes

#### Scenario: Original attachment gone
- **WHEN** a forwarded attachment no longer exists in Gmail at send time
- **THEN** the compose window shows "Couldn't attach <file>: The attachment is no longer in
  Gmail." and the message is not sent

### Requirement: Attachments in demo mode
In demo mode Save SHALL show the notice "Demo mode doesn't save files" and write nothing.
Attaching and forwarding SHALL work: local files are read when the message is sent, forwarded
attachments use the generated mailbox's own data, and the sent message lists them.

#### Scenario: Save in demo mode
- **WHEN** a demo script runs `open:N` on a thread with an attachment, then `save:0` and
  `expect:notice=Demo mode doesn't save files`
- **THEN** the expectation passes and no file is written

#### Scenario: Scripted attach
- **WHEN** a demo script presses `c`, then runs `attach:<path>` for a small file and
  `expect:attached=1`
- **THEN** the expectation passes

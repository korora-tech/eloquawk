# summary-cache Specification

## Purpose
Show each account's mail list straight away on launch, without refetching every inbox
thread's metadata, while keeping mail off the disk in any readable form. Implementation
detail lives in docs/SPEC.md section 7.7.

## Requirements

### Requirement: Per-account snapshot of listings
The app SHALL keep, per signed-in account, a snapshot of the account's labels with their
last counts, page 1 of each label view the user listed (Inbox, user labels and All Mail)
and the thread summaries those pages show.

#### Scenario: Relaunch shows the last listing before sync answers
- **WHEN** the user relaunches the app with a saved snapshot and an unlocked keyring
- **THEN** the label list and the last-listed page of each cached view appear from the snapshot
- **AND** the views still show as loading until the live listing replaces them

#### Scenario: Unchanged threads are not fetched again
- **WHEN** initial sync lists a thread whose history id matches the snapshot
- **THEN** the cached summary is reused instead of fetching the thread's metadata

#### Scenario: Searches are never cached
- **WHEN** the user runs a search and then relaunches
- **THEN** no search results appear from the snapshot

### Requirement: No mail content in the snapshot
The snapshot SHALL NOT contain message bodies, attachments or draft text.

#### Scenario: Opened thread
- **WHEN** the user opens and reads a thread, then the snapshot is saved
- **THEN** the saved snapshot holds the thread's summary only, not its body

### Requirement: Encrypted at rest with a keyring-held key
The snapshot SHALL be encrypted and authenticated with a key held only in the Secret
Service, bound to the account it belongs to, and readable only by the user's account on
the machine.

#### Scenario: No keyring
- **WHEN** no Secret Service is available
- **THEN** the app keeps no snapshot and syncs from scratch on every launch

#### Scenario: Unreadable key
- **WHEN** the cache key cannot be read from the keyring
- **THEN** the app makes a new key, and existing snapshots read as no cache

#### Scenario: Tampered, foreign or old-format file
- **WHEN** a snapshot fails to decrypt or authenticate, belongs to another account, or
  uses an old format
- **THEN** it reads as no cache and the account runs a full initial sync

### Requirement: Snapshot lifecycle
The app SHALL save the snapshot when the sync queue drains with cached data changed and
at shutdown, SHALL empty it when an account is reset, and SHALL delete it when an account
is removed.

#### Scenario: Account reset
- **WHEN** Gmail reports that an account's history is too old and the account resets
- **THEN** the account's snapshot is emptied

#### Scenario: Signed-out account
- **WHEN** the user removes an account, even when its cache key is unreadable
- **THEN** the account's snapshot file is deleted

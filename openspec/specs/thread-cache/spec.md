# thread-cache Specification

## Purpose
Show a thread the user already opened without a request, and let it be read offline,
while keeping mail off the disk in any readable form. Implementation detail lives in
docs/SPEC.md section 7.8.

## Requirements

### Requirement: Opened threads are kept, sealed
The app SHALL keep each thread it fetched in full, bodies included, encrypted and
authenticated with the summary cache's keyring-held key, bound to its account and thread,
readable only by the user's account on the machine.

#### Scenario: Nothing readable on disk
- **WHEN** a thread is opened and saved
- **THEN** no file or file name holds its subject, body, addresses or ids in the clear

#### Scenario: Moved or tampered file
- **WHEN** a saved thread fails to authenticate, belongs to another account or thread, or uses an old format
- **THEN** it reads as not saved and the thread is fetched

#### Scenario: No keyring
- **WHEN** no Secret Service is available
- **THEN** no thread is saved

### Requirement: Unchanged threads open without a request
The app SHALL show a saved thread without a request while the list's history id for it equals the one it was saved at.

#### Scenario: Reopen
- **WHEN** the user opens a thread saved at the history id the list shows
- **THEN** the reader shows it with no network request

#### Scenario: Changed thread
- **WHEN** the list shows a newer history id than the saved copy
- **THEN** the thread is fetched again and the saved copy replaced

### Requirement: Offline reading
The app SHALL show a saved thread when fetching it fails, rather than an error, unless Gmail says the thread no longer exists.

#### Scenario: No connection
- **WHEN** the user opens a saved thread that changed since, and Gmail cannot be reached
- **THEN** the saved copy is shown

#### Scenario: Never opened
- **WHEN** the user opens a thread that was never saved, with no connection
- **THEN** the reader shows the failure as before

### Requirement: Bounded and removable
The store SHALL keep at most 1500 threads and 192 MiB per account, dropping the least recently written first; SHALL delete a thread Gmail reports gone; SHALL empty an account's store when it resets; and SHALL delete it when the account is removed, even when the key is unreadable.

# accounts-and-signin Specification

## Purpose
Sign Google accounts in and out through an OAuth client, keep their credentials only in the
system keyring, and tell the user plainly when an account needs attention. Implementation
detail lives in docs/SPEC.md sections 1, 6.3, 6.4 and 7.6, milestone M9, and
docs/google-gmail.md.

## Requirements

### Requirement: Built-in client with overrides
The app SHALL ship with its own Google Desktop OAuth client, so most users only need to sign
in. A build MAY replace each half of it at compile time, and a client the user saves in
Settings SHALL win over the compiled-in one. The "Set up a Google client" card SHALL appear
only when the build has no usable client and none is saved.

#### Scenario: Default build
- **WHEN** the user launches a default build with an empty keyring
- **THEN** the "Sign in to Google" card appears, with a "Sign in with Google" button

#### Scenario: Build without a client
- **WHEN** the app was built with the client id or secret set to an empty string and no client is saved
- **THEN** the "Set up a Google client" card appears, offering "Open Settings" and "Setup guide"

### Requirement: Browser sign-in with PKCE and a loopback redirect
Sign-in SHALL open Google's account picker and consent page in the browser, using an
authorisation code with PKCE and a one-shot redirect to `http://127.0.0.1` on a random port.
It SHALL ask for offline access and the one scope `https://www.googleapis.com/auth/gmail.modify`
only, and SHALL take the account's address from the Gmail profile. While it waits the app
SHALL offer "Copy sign-in link" and "Cancel", and SHALL give up after five minutes.

#### Scenario: Successful sign-in
- **WHEN** the user approves access in the browser
- **THEN** the browser tab says "Eloquawk is connected to Gmail. You can close this tab."
- **AND** the app shows the notice "Added <email>" and starts syncing that account

#### Scenario: Forged or stale redirect
- **WHEN** a request reaches the loopback port with a state that is not the one the app is waiting for
- **THEN** the page says "This sign-in link is not the one Eloquawk is waiting for."
- **AND** the app keeps waiting for the real redirect

#### Scenario: Signing in again
- **WHEN** the user signs in to an account that is already present
- **THEN** its refresh token is replaced without revoking the old grant
- **AND** the app shows "<email> signed in again"

### Requirement: Gmail permission must be granted
If Google's answer does not include the Gmail scope, the app SHALL save nothing and SHALL say
"Eloquawk needs permission to read and organise your mail. Sign in again and allow Gmail
access on Google's consent page." It SHALL revoke the useless grant only when no account is
signed in, since revoking could otherwise sign out an existing account.

#### Scenario: Gmail box unticked on the consent page
- **WHEN** the user approves sign-in with the Gmail permission unticked
- **THEN** no account is added and the keyring is unchanged
- **AND** the missing-permission message appears

### Requirement: Credentials only in the system keyring
Refresh tokens and a user-saved OAuth client SHALL be stored only in the Secret Service under
the service `korora.tech.Eloquawk`, as the items "Eloquawk: Google accounts" and "Eloquawk:
OAuth client". Access tokens SHALL live only in memory. No credential SHALL be written to a
file or to the log, and an unreadable keyring entry SHALL NOT be quoted in any message. An
in-memory credential store SHALL be available only to debug builds.

#### Scenario: Locked keyring
- **WHEN** the app starts and the keyring is locked or unavailable
- **THEN** the "Unlock your keyring" card appears with a Retry button
- **AND** after the user unlocks it, Retry or Ctrl+R loads the accounts

#### Scenario: Unreadable saved sign-ins
- **WHEN** the stored refresh tokens cannot be parsed
- **THEN** they are reset and the banner says "Stored sign-ins were unreadable and were reset; add your accounts again."

#### Scenario: Keyring write fails during sign-in
- **WHEN** a sign-in succeeds but the token cannot be written to the keyring
- **THEN** the account works and the banner says it is signed in for this session only

### Requirement: Saving and clearing a client
Settings SHALL let the user paste a client ID and secret and "Save to keyring". The secret
field SHALL never be filled from the keyring; leaving it blank SHALL keep the stored secret
for the same client ID. Saving a client with a different ID SHALL sign out every account, and
SHALL do so only after the keyring write succeeds. A new secret for the same ID SHALL keep the
accounts. "Use built-in client" SHALL delete the saved client and every account.

#### Scenario: Switching clients
- **WHEN** the user saves a client whose ID differs from the one in use
- **THEN** the notice "OAuth client saved to the keyring" appears and every account is signed out

#### Scenario: Keyring write fails
- **WHEN** saving a client fails to write to the keyring
- **THEN** the old client and its accounts stay as they were and the error is shown

#### Scenario: Back to the built-in client
- **WHEN** the user clicks "Use built-in client"
- **THEN** the saved client and every account are removed
- **AND** the notice "Your OAuth client was removed; using the built-in client" appears

### Requirement: Several accounts
The user SHALL be able to add more accounts from Settings with "Add account", and each SHALL
sync independently. "Remove" next to an account SHALL sign it out, revoke its token with
Google, delete it from the keyring and delete its summary cache.

#### Scenario: Remove an account
- **WHEN** the user clicks Remove next to an account in Settings
- **THEN** the notice "Signed out of <email>" appears and the account leaves the sidebar

#### Scenario: Google cannot be told
- **WHEN** the user removes an account and the revoke call fails
- **THEN** the account is still removed locally
- **AND** the notice says Google could not be told and points to myaccount.google.com/permissions

### Requirement: Expired or revoked sign-in drops only that account
When Google rejects an account's refresh token, the app SHALL remove that account locally,
keep the others syncing, and leave the sticky banner message "Google signed <email> out. Add
it again." followed by "(Your organisation requires you to sign in again.)" when Google asked
for reauthentication, and "(Test-mode tokens expire after 7 days.)" otherwise. A rejected
client SHALL sign out every account and say so. Access tokens SHALL be refreshed when less
than a minute remains, and a call that Google answers as unauthorised SHALL be retried once
with a fresh token.

#### Scenario: Seven-day test token expires
- **WHEN** one of two accounts has its refresh token expire
- **THEN** that account disappears and the banner shows "Google signed <email> out. Add it again. (Test-mode tokens expire after 7 days.)"
- **AND** the other account keeps syncing

#### Scenario: Token endpoint unreachable
- **WHEN** the token endpoint cannot be reached or answers with a server error
- **THEN** no account is removed and the banner shows "Couldn't reach Google. Check your connection; Ctrl+R tries again."

### Requirement: Sign-in status is visible
The sidebar SHALL show the sign-in state: "No Google client", "Waiting for browser…", "Not
signed in", the one account's address, or "<n> Google accounts".

#### Scenario: Waiting for the browser
- **WHEN** the user has started a sign-in and not finished it
- **THEN** the sidebar shows "Waiting for browser…"

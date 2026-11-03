# settings-and-theme Specification

## Purpose
Let the user change Eloquawk's preferences on a Settings screen, keep them between launches
and pick a light, dark or system-following theme. Implementation detail lives in docs/SPEC.md
sections 4.3, 8.3 and 8.4 and milestone M9. The "Message checks (OpenRouter)" card belongs to
the message-checks capability, and the reader's message-order option to the reader capability.
The OAuth client and Accounts cards are described here as screens; what signing in, saving a
client and removing an account do belongs to the accounts-and-signin capability.

## Requirements

### Requirement: Settings screen
The app SHALL show Settings in the main window when the user presses ctrl-, , clicks the
header's Settings button or picks Settings in the command palette. Settings SHALL hold the
cards "Google OAuth client", "Accounts", "Preferences" and "Message checks (OpenRouter)", and
Escape or ✕ SHALL return to the mail view.

#### Scenario: Open and close
- **WHEN** the user presses ctrl-, in the mail view
- **THEN** the screen shows Settings
- **AND** pressing Escape shows the mail view again

### Requirement: Preferences and their defaults
The Preferences card SHALL offer Theme as System, Light or Dark (default System), "24-hour
clock" (default on), "Mark as read when opened" (default on), "Show the latest message
first" (default on, see the reader capability), "Notify about new mail" (default on) and
"Check for new mail every" 1, 5 or 15 min (default 1 min). A change SHALL take effect at once,
without a restart.

#### Scenario: Fresh install
- **WHEN** the user opens Settings with no saved settings
- **THEN** Theme is System, the four checkboxes are ticked and 1 min is selected

#### Scenario: 12-hour clock
- **WHEN** the user unticks "24-hour clock"
- **THEN** times in the mail list and reader show in 12-hour form

#### Scenario: Leave unread on open
- **WHEN** the user unticks "Mark as read when opened" and opens an unread conversation
- **THEN** the conversation stays unread

#### Scenario: Notifications off
- **WHEN** the user unticks "Notify about new mail" and new mail arrives
- **THEN** no desktop notification is shown

### Requirement: Value ranges
The time between checks for new mail SHALL be held between 30 seconds and 15 minutes, and
the number of threads per list page between 10 and 100 (default 50); a value outside its
range, whether loaded or set, SHALL be pulled to the nearest end.

#### Scenario: Out-of-range saved value
- **WHEN** the saved settings ask for a check every 5 seconds and 500 threads per page
- **THEN** the app checks every 30 seconds and lists 100 threads per page

### Requirement: Saved between launches
Every change SHALL be saved straight away, replacing the settings file whole so that a crash
cannot leave it half-written. A missing or unreadable settings file SHALL give the defaults.
The app SHALL also restore the last label view the user showed (never a search), the
collapsed accounts and label trees in the sidebar and whether the sidebar is folded. In
demo mode nothing SHALL be saved. The settings SHALL hold no credentials and no mail.

#### Scenario: Restart keeps a change
- **WHEN** the user picks Light, quits and relaunches
- **THEN** the app starts in the light theme with Light selected

#### Scenario: Last view restored
- **WHEN** the user shows the Travel label, runs a search, quits and relaunches
- **THEN** the app opens on the Travel label

#### Scenario: Corrupt file
- **WHEN** the settings file is not valid JSON
- **THEN** the app starts with the default settings

### Requirement: Theme
Light and Dark SHALL fix the palette. System SHALL follow the desktop's light or dark
appearance, including when the desktop switches while the app runs. A theme change SHALL
redraw every open window, compose windows included.

#### Scenario: Desktop switches to dark
- **WHEN** Theme is System and the desktop changes from light to dark
- **THEN** every Eloquawk window switches to the dark palette without a restart

### Requirement: Session theme override
`--theme light`, `--theme dark` or `--theme system` SHALL set the theme for that run only,
without saving it. Picking a theme in Settings SHALL replace the override and be saved.

#### Scenario: Flag is not saved
- **WHEN** the saved theme is Dark and the user launches with `--theme light`, then quits and
  launches without the flag
- **THEN** the first run is light and the second is dark

#### Scenario: Settings replaces the flag
- **WHEN** the user launches with `--theme light` and picks System in Settings
- **THEN** the app follows the desktop appearance from then on

### Requirement: Readable palettes
In both palettes, body text, secondary text, accent text and links SHALL have a contrast ratio
of at least 4.5 against the window background, panels and the selected row, and muted text and
captions at least 3.0.

#### Scenario: Contrast check
- **WHEN** the palette tests run
- **THEN** every listed pairing meets its ratio in the light and the dark palette

### Requirement: OAuth client card
The "Google OAuth client" card SHALL offer a client ID field and a masked client secret field
with Show and Hide, "Save to keyring", and "Use built-in client" when a client is saved and the
build has a built-in one. It SHALL link to the Gmail API page and the setup guide and SHALL
show where the client stands, such as "Using the built-in client" or "Your client is stored
in the keyring". After a save the secret field SHALL be emptied and masked; saving with the
secret left blank SHALL keep the stored secret.

#### Scenario: Secret is not kept in the field
- **WHEN** the user enters a client ID and secret and clicks "Save to keyring"
- **THEN** the secret field is empty and masked again

### Requirement: Accounts card
The "Accounts" card SHALL list the signed-in accounts, each with Remove, and SHALL offer "Sign
in with Google" when there is none and "Add account" otherwise. Removing an account SHALL sign
it out. In demo mode, sign-in and removal SHALL be refused with the notice "Demo mode:
sign-in is disabled".

#### Scenario: Demo accounts
- **WHEN** the user clicks Remove on an account in demo mode
- **THEN** the notice "Demo mode: sign-in is disabled" shows and the account stays

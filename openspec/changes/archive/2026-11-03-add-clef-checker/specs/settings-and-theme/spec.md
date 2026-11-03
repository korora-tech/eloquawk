## MODIFIED Requirements

### Requirement: Settings screen
The app SHALL show Settings in the main window when the user presses ctrl-, , clicks the
header's Settings button or picks Settings in the command palette. Settings SHALL hold the
cards "Google OAuth client", "Accounts", "Preferences" and "Message checks", and Escape or ✕
SHALL return to the mail view.

#### Scenario: Open and close
- **WHEN** the user presses ctrl-, in the mail view
- **THEN** the screen shows Settings
- **AND** pressing Escape shows the mail view again

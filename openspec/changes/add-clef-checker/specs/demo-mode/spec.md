## MODIFIED Requirements

### Requirement: Offline and private
In demo mode the app SHALL NOT contact Google, OpenRouter or Cloudflare and SHALL NOT open
the Secret Service. It SHALL read the saved settings but never write them, keep no summary
cache and save no attachments. Sign-in, client and account commands SHALL be refused with the
notice "Demo mode: sign-in is disabled". Message checks SHALL use a built-in offline checker
whatever checker is chosen. Desktop notifications SHALL still be shown.

#### Scenario: Settings change in demo
- **WHEN** the user picks Light in Settings in demo mode and relaunches without `--demo`
- **THEN** the theme is the one saved before the demo run

#### Scenario: No keyring prompt
- **WHEN** the app starts in demo mode with the keyring locked
- **THEN** no unlock prompt appears and the generated mailbox shows

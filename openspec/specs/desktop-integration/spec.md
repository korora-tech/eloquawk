# desktop-integration Specification

## Purpose
Make Eloquawk a well-behaved Linux desktop app: one app id, a valid desktop entry and icons,
registration as a `mailto:` handler, and a Nix package with a Home Manager module.
Implementation detail lives in docs/SPEC.md milestone M14 and docs/desktop-integration.md.
Notifications and the single-instance hand-off belong to their own capability.

## Requirements

### Requirement: One app id everywhere
The app id `korora.tech.Eloquawk` SHALL be used as the Wayland app id of every window, the
desktop entry's file name, its `Icon` and `StartupWMClass`, the installed icon names, the
notification app icon and desktop-entry hint, and the keyring service.

#### Scenario: Window matched to its entry
- **WHEN** the user launches Eloquawk on Wayland
- **THEN** its windows carry app id `korora.tech.Eloquawk`, which the desktop matches to the
  installed entry and icon

### Requirement: Desktop entry
The package SHALL install `share/applications/korora.tech.Eloquawk.desktop`, an
`Application` entry named "Eloquawk" with `GenericName=Mail Client`, `Exec=eloquawk %u`,
`Icon=korora.tech.Eloquawk`, `Terminal=false`, `Categories=Network;Email;`, Gmail and mail
`Keywords`, `MimeType=x-scheme-handler/mailto;`, `StartupNotify=true` and
`StartupWMClass=korora.tech.Eloquawk`. It SHALL pass `desktop-file-validate`.

#### Scenario: Validation
- **WHEN** `nix flake check` runs
- **THEN** the installed entry passes `desktop-file-validate`

### Requirement: App icons
The package SHALL install the app icon as
`share/icons/hicolor/scalable/apps/korora.tech.Eloquawk.svg` and a 512×512
`share/icons/hicolor/512x512/apps/korora.tech.Eloquawk.png`. The app SHALL show the same
icon in its header and above the sign-in card.

#### Scenario: Icons installed
- **WHEN** the user builds the package with `nix build`
- **THEN** both hicolor icons exist and are not empty

### Requirement: Mailto handler
The desktop entry SHALL register Eloquawk for `x-scheme-handler/mailto`, and
`eloquawk mailto:…` SHALL open a compose window prefilled from the link. Only the recipients
and the `to`, `cc`, `bcc`, `subject` and `body` fields SHALL be read; `attach`, `from`,
`in-reply-to`, `references` and every other field SHALL be ignored. The message SHALL be sent
from the account the user is looking at and SHALL never be sent without the user pressing
Send. Before any account is ready, the link SHALL wait with the notice "Sign in to compose
this message" and open once an account is ready. Installing the app SHALL NOT make it the
default mail handler; the user chooses that in desktop settings, with `xdg-mime` or in Home
Manager.

#### Scenario: Link from a browser
- **WHEN** Eloquawk is the default handler and the user clicks
  `mailto:ana@example.com?subject=Hello`
- **THEN** a compose window opens addressed to ana@example.com with the subject "Hello" and
  nothing is sent

#### Scenario: Link asking for an attachment
- **WHEN** a `mailto:` link carries `attach=/etc/passwd`
- **THEN** the compose window has no attachment

#### Scenario: Not signed in
- **WHEN** a `mailto:` link arrives before any account is signed in
- **THEN** the notice "Sign in to compose this message" shows
- **AND** the compose window opens once an account is ready

### Requirement: Nix package
The flake SHALL provide `packages.<system>.default` and `apps.<system>.default` for
`x86_64-linux` and `aarch64-linux`, building the `eloquawk` binary with the desktop entry,
icons and licence. The binary SHALL be wrapped so GPUI finds Vulkan, Wayland and xkbcommon at
runtime and so Inter is available through fontconfig, while a `FONTCONFIG_FILE` the user
already set wins.

#### Scenario: Build and run
- **WHEN** the user runs `nix run github:korora-tech/eloquawk -- --demo`
- **THEN** Eloquawk builds and starts in demo mode

### Requirement: Home Manager module
The flake SHALL export `homeModules.default` (also as `homeModules.eloquawk` and under the
older name `homeManagerModules`) with the options `programs.eloquawk.enable` and
`programs.eloquawk.package`, defaulting to this flake's package. Enabling it SHALL install
the package, which brings the desktop entry, the `mailto:` registration and the icons, and
SHALL do nothing else: no service, no default-handler setting, and no management of the app's
own `settings.json`. A flake check SHALL evaluate the module.

#### Scenario: Enabled
- **WHEN** `programs.eloquawk.enable = true`
- **THEN** `home.packages` holds the Eloquawk package

#### Scenario: Disabled
- **WHEN** the module is imported and `enable` is not set
- **THEN** nothing is installed

#### Scenario: Custom package
- **WHEN** `programs.eloquawk.package` is set to another package
- **THEN** that package is installed instead

### Requirement: No background presence
Eloquawk SHALL have no tray icon and no background service. Closing the last window and
`Ctrl+Q` SHALL quit the app.

#### Scenario: Last window closed
- **WHEN** the user closes the last Eloquawk window
- **THEN** the process exits

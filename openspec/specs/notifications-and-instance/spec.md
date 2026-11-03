# notifications-and-instance Specification

## Purpose
Tell the user about new mail with desktop notifications, keep one Eloquawk running per user,
and hand `mailto:` links and repeat launches to the running instance. Implementation detail
lives in docs/SPEC.md sections 7.4 and 11 and milestones M13 and M14. Which mail counts as new
is decided by the gmail-sync capability.

## Requirements

### Requirement: Desktop notifications for new mail
The app SHALL show a freedesktop notification, with the app's icon and the category
`email.arrived`, for each conversation a change check reports as new mail, while "Notify about
new mail" is on in Settings (on by default). An account's first sync SHALL NOT notify, so
starting the app never floods the user. A conversation SHALL notify again only when a newer
message arrives in it, and a conversation no longer unread in the Inbox SHALL NOT notify.

#### Scenario: Quiet start
- **WHEN** the user launches the app with unread mail in the Inbox
- **THEN** no notification is shown

#### Scenario: Simulated new mail in demo mode
- **WHEN** the demo runs with notifications on
- **THEN** exactly one notification is shown about 20 seconds after launch

#### Scenario: Notifications off
- **WHEN** the user turns off "Notify about new mail" and new mail arrives
- **THEN** no notification is shown

#### Scenario: Same conversation, same message
- **WHEN** a conversation that has already notified is reported again with no newer message
- **THEN** no second notification is shown

### Requirement: Notification content
Up to three new conversations at once SHALL each give one notification, newest first, with the
sender as the title and the subject ("(no subject)" when empty) and the start of the snippet,
cut to 140 characters, as the body. More than three at once SHALL give a single "<n> new
conversations" notification naming up to three senders, as in "From A, B, C and 2 others".
Mail text SHALL be escaped in the body and stripped of control and bidirectional-override
characters, so a subject cannot inject markup or reorder the text.

#### Scenario: A burst of new mail
- **WHEN** five new conversations from five senders are reported at once
- **THEN** one notification titled "5 new conversations" is shown, with the body "From <a>, <b>, <c> and 2 others"

#### Scenario: Markup in a subject
- **WHEN** a new message's subject contains `<b>`
- **THEN** the notification body shows the text `<b>` rather than bold text

### Requirement: One instance per user
A normal launch SHALL take a lock and a socket in the user's private `$XDG_RUNTIME_DIR`, both
readable only by the user. A later launch that finds a running Eloquawk SHALL hand its request
over and exit. When `$XDG_RUNTIME_DIR` is unset, not absolute, not owned by the user or not
private, the app SHALL run on its own without single-instance support and SHALL NOT fall back
to `/tmp`. Demo mode SHALL stay out of the single instance unless started with `--instance`,
and then SHALL use names of its own so it never meets a real instance.

#### Scenario: Second launch
- **WHEN** Eloquawk is running and the user launches it again
- **THEN** the second process hands over to the first and exits

#### Scenario: Demo beside a real instance
- **WHEN** a real instance is running and the user starts `eloquawk --demo --instance`
- **THEN** the demo starts as its own instance

#### Scenario: No runtime directory
- **WHEN** the app starts with `XDG_RUNTIME_DIR` unset
- **THEN** it runs without single-instance support and creates nothing in `/tmp`

### Requirement: Recovering a stale or exiting instance
A normal quit SHALL remove the socket and release the lock, so the next launch becomes the
instance at once. A socket left by a crash SHALL be replaced by the next launch. A launch
that finds the lock held SHALL keep trying for 3 seconds and take over if the holder exits in
that time; a holder that stays unresponsive for longer SHALL leave the new launch running alone.

#### Scenario: After a crash
- **WHEN** the previous Eloquawk was killed and left its socket behind
- **THEN** the next launch replaces the socket and becomes the instance

### Requirement: Raising the running window
A repeat launch with no `mailto:` link SHALL ask the running instance to show its main window,
reopening it if only compose windows are open. The launch SHALL pass on its
`XDG_ACTIVATION_TOKEN`, and the running instance SHALL spend it to raise and focus the window.
Without a token the request SHALL still be handed over, and the compositor decides whether the
window is raised.

#### Scenario: Launch from a launcher or key binding
- **WHEN** the user starts Eloquawk from a launcher while it is already running
- **THEN** the existing main window is raised and focused and no second window opens

### Requirement: mailto links open a compose window
The app SHALL accept one positional `mailto:` URL and register as the desktop's `mailto`
handler. The link SHALL open a compose window, in the running instance if there is one, from
the current account. Only `to`, `cc`, `bcc`, `subject` and `body` SHALL be taken from the link;
attachments, the sender, threading headers and any other field SHALL be ignored, and nothing
SHALL be sent until the user presses Send. More than one `mailto:` argument SHALL be refused.

#### Scenario: Attachment field in a link
- **WHEN** Eloquawk is running and the user runs `eloquawk 'mailto:x@y?subject=Hi&attach=/etc/passwd'`
- **THEN** a compose window opens in the running instance addressed to x@y with the subject "Hi"
- **AND** it has no attachment

#### Scenario: No account yet
- **WHEN** a `mailto:` link arrives before any account is signed in
- **THEN** the link waits and opens once the first account signs in
- **AND** once the app knows there is no account, the main window is raised with the notice "Sign in to compose this message"

#### Scenario: Not a mailto URL
- **WHEN** a hand-off asks the running instance to compose something that is not a `mailto:` URL
- **THEN** the request is refused and no window opens

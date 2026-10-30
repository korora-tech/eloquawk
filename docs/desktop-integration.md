# Desktop integration

[← Back to the README](../README.md)

## Everyday use

- **Panes:** accounts and labels on the left, the conversation list in the middle, the open conversation on the right. Clicking an account's address collapses or expands it; a collapsed account shows its inbox unread count. The panes have fixed widths.
- **Reading:** unread messages and the last message of a conversation open expanded; click a header (or press `o` on the focused message) to toggle one, `;` to expand all and `:` to collapse all. Quoted text folds behind **• • •**. HTML mail is shown converted to text, with a link's real address next to its text, so a misleading link is easy to spot. Remote images are never loaded. **Open in Gmail** opens the conversation in your browser.
- **Selecting text:** drag across a message to select, double-click for a word, `Ctrl+C` to copy, `Ctrl+A` for the whole message. **Copy** on a message copies all of it. A selection stays within one message.
- **Links:** `http`, `https` and `www.` links open in your browser, and `mailto:` links open a new message. No other kind of link is ever opened.
- **Several conversations:** `x` (or the checkbox on a row) selects conversations; archive, trash, spam, star, read state and labels then apply to all of them.
- **Attachments:** **Save** writes the file to your download directory: `XDG_DOWNLOAD_DIR` from `~/.config/user-dirs.dirs` when it names an absolute path, otherwise `~/Downloads`. Names are cleaned of path separators and control or direction-changing characters, and an existing file is never overwritten (`name (1).ext` instead). The notice offers **Open folder**.
- **Composing:** each message opens in its own window. Cc and Bcc hide behind **Cc Bcc** until used. **Attach…** uses your desktop's file chooser portal; attachments may total 25 MB. Closing a message you have changed asks before discarding it.
- **Theme:** System, Light or Dark in Settings. `eloquawk --theme light|dark|system` overrides it for one session.
- **Closing:** there is no tray icon or background service. Closing the last window quits, and so does `Ctrl+Q`.

## Mailto links

The desktop entry (`korora.tech.Eloquawk.desktop`) registers Eloquawk for `x-scheme-handler/mailto`. Most desktops let you choose it as the default mail app in their settings; otherwise:

```sh
xdg-mime default korora.tech.Eloquawk.desktop x-scheme-handler/mailto
```

A link such as `mailto:ana@example.com?subject=Hello` then opens a new message in Eloquawk (`eloquawk mailto:…` does the same from a terminal).

- **What is read from the link:** only the recipients and the `to`, `cc`, `bcc`, `subject` and `body` fields. Everything else, including `attach`, `attachment`, `from`, `in-reply-to` and `references`, is ignored, so a link can never attach a file from your disk or make a message look like a reply.
- **Never sent automatically:** a link only fills in a compose window; you still choose Send.
- **From:** the account you are looking at.
- **Before you sign in:** the link waits, a "Sign in to compose this message" notice says so, and the message opens once an account is ready.

## Notifications

New mail is announced through the freedesktop notification service (`org.freedesktop.Notifications`), with the app icon and the `email.arrived` category. Each notification shows the sender as its title and the subject and start of the message as its body, escaped so that mail text cannot inject markup. More than three new conversations at once become a single summary. See [which mail notifies](google-gmail.md#which-mail-notifies) for the rules, and turn them off in **Settings → Notify about new mail**.

## Single instance

Only one Eloquawk runs per session. The first takes a lock on `$XDG_RUNTIME_DIR/eloquawk.lock` and listens on the private socket `$XDG_RUNTIME_DIR/eloquawk.sock`. A later launch prints `eloquawk: already running (pid N); handing over`, asks the running one to bring its window forward, or to open a compose window for its `mailto:` link, and exits. On Wayland it passes along the launcher's activation token, so the existing window may take focus.

- Without `XDG_RUNTIME_DIR` there is no lock or socket, so two instances can run.
- `--demo` runs alongside a real instance and never answers its hand-offs. `--demo --instance` tests the hand-off with its own names (`eloquawk-demo.lock` and `eloquawk-demo.sock`).

## Home Manager

Add the repository to your flake's inputs:

```nix
inputs.eloquawk.url = "github:korora-tech/eloquawk";
```

Pass `eloquawk` to your Home Manager modules with `extraSpecialArgs = { inherit eloquawk; };` (or `home-manager.extraSpecialArgs` when using the NixOS module). Then add:

```nix
{ eloquawk, ... }:
{
  imports = [ eloquawk.homeModules.default ];
  programs.eloquawk = {
    enable = true;                  # install the package
    # package = eloquawk.packages.${pkgs.system}.default;  # the default
  };
}
```

The module installs the package, which brings the desktop entry, the `mailto:` registration and the icons. There is no service to enable, because Eloquawk has no tray and does not run in the background. It does not set Eloquawk as your default mail handler; use your desktop's settings, `xdg-mime` as above, or Home Manager's `xdg.mimeApps.defaultApplications."x-scheme-handler/mailto"`. Eloquawk's own `settings.json` is not managed: the app writes it.

`homeManagerModules.default` is the same module under the older output name.

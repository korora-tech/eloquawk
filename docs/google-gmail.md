# Gmail setup

[← Back to the README](../README.md)

Eloquawk talks to Gmail through a Google OAuth client. It ships with one of its own, so most people only need to sign in. This guide covers tester access for that client, setting up a client of your own, where your credentials are kept, how sync works and which mail notifies.

## Request tester access

**Gmail access through Eloquawk's built-in client is currently available only to approved testers.** Email [david@rawkode.academy](mailto:david@rawkode.academy?subject=Eloquawk%20tester%20access) with the Google account address you want to connect and ask to be added. Wait for access to be confirmed before signing in. Each Google account you connect needs to be on the tester list.

You can use [demo mode](../README.md#try-it-without-a-google-account) straight away, without joining the test or signing in.

### Testing mode and the seven-day sign-in

Eloquawk asks for one scope, `https://www.googleapis.com/auth/gmail.modify`. It covers reading mail, changing labels, archiving, trashing and sending, but not permanent deletion. Google classes it as a **restricted** scope: an app may only offer it to the public after a security assessment. Until then the Google project stays in **Testing**, which means:

- Only accounts on the project's test-user list can sign in. Google shows them an "unverified app" warning first; choose **Continue**.
- Google expires the sign-in (the refresh token) **seven days** after consent. Eloquawk then drops that account and says "Google signed you@example.com out. Add it again. (Test-mode tokens expire after 7 days.)" Add it again from Settings. Your other accounts keep syncing.

See [Google's testing-mode documentation](https://support.google.com/cloud/answer/15549945?hl=en). The same limits apply to a client of your own while its project is in Testing.

## Sign in

1. Click **Sign in with Google**. Your browser opens Google's account picker and consent page. Approve access, then return to Eloquawk. If no browser opens, **Copy sign-in link** and open it yourself.
2. To add more accounts, open **Settings** (`Ctrl+,`) and click **Add account**. Each account appears in the sidebar under its address.
3. **Remove** next to an account in Settings signs it out, revokes its token with Google and deletes it from the keyring.

On the consent page, leave the Gmail permission ticked. If it is not granted, Eloquawk saves nothing and asks you to sign in again.

## Using your own OAuth client

Use your own client when you are not on the tester list, or want your own access list. A client of yours manages its test users separately from Eloquawk's.

1. In [Google Cloud](https://console.cloud.google.com/), create a project (or pick one).
2. [Enable the Gmail API](https://console.cloud.google.com/apis/library/gmail.googleapis.com) for it. Settings has an **Enable the Gmail API** button that opens this page.
3. Set up the consent screen under [Google Auth Platform](https://console.cloud.google.com/auth/overview):
   - **Branding:** an app name and your support email.
   - **Audience:** User type **External**, publishing status **Testing**. Under **Test users**, add every Google account you will sign in with.
   - **Data access:** add the scope `https://www.googleapis.com/auth/gmail.modify`. Google lists it as restricted; that is expected, and fine for a project in Testing.
4. Under [Clients](https://console.cloud.google.com/auth/clients), create an OAuth client with application type **Desktop app**. Copy its client ID and client secret.
5. In Eloquawk, open **Settings**, paste the client ID and secret under **Google OAuth client**, and click **Save to keyring**. Then sign in as above.

The client must be a **Desktop app**. It has no redirect URI setting: Google accepts any `http://127.0.0.1:<port>` redirect for desktop clients. If Google reports `redirect_uri_mismatch`, you created a Web application client instead.

A Desktop client's secret cannot be kept confidential, and Google does not treat it as one; PKCE protects the sign-in. That is why the built-in client is compiled into the binary.

### Building with your own client

A build can replace the built-in client by setting these at compile time:

```sh
ELOQUAWK_GOOGLE_CLIENT_ID=1234-abc.apps.googleusercontent.com \
ELOQUAWK_GOOGLE_CLIENT_SECRET=GOCSPX-... \
  nix develop -c cargo build --release
```

Each variable replaces its half of the built-in client. Setting either one to an empty string builds Eloquawk without a client: it then opens on a **Set up a Google client** card until you save one in Settings. A client saved in Settings always wins over the compiled-in one.

### How sign-in works

Sign-in uses Google's installed-app flow: an authorisation code with PKCE, and a one-shot loopback redirect to `http://127.0.0.1:<random port>` that closes after the first request (or after five minutes). Eloquawk asks for offline access, so Google returns a refresh token, and reads the account's address from the Gmail profile.

## Where credentials live

| What | Where |
|---|---|
| Your own OAuth client ID and secret (if saved) | Keyring item `Eloquawk: OAuth client` (service `korora.tech.Eloquawk`) |
| Refresh tokens, one per account | Keyring item `Eloquawk: Google accounts`, a JSON map of address to token |
| Access tokens | Memory only |
| Mail | Memory only, never written to disk or logs. An attachment you save is the one exception. |

- **Changing the client:** saving a client with a different ID (including your first one, replacing the built-in client) signs out every account, because a token belongs to the client that issued it. Add your accounts again afterwards. A new secret for the same client ID keeps them.
- **Use built-in client:** deletes your saved client and every account, and goes back to the built-in client.
- **Client secret field:** never filled in from the keyring. Leave it blank to keep the stored secret for the same client ID.
- **Expired or revoked sign-in:** only that account is dropped and named in the banner; the others keep syncing. A Google Workspace organisation that requires you to sign in again shows "(Your organisation requires you to sign in again.)" instead of the seven-day hint.

Preferences that are not secret (theme, 24-hour clock, mark as read when opened, notifications, refresh interval, collapsed accounts and the last label you looked at) are saved in `$XDG_CONFIG_HOME/eloquawk/settings.json`. Demo mode never writes this file.

## How sync works

Eloquawk keeps nothing on disk, so it fetches what it shows each time it starts, and one background worker makes every request to Gmail, one at a time.

- **First sync per account:** the profile (whose history ID becomes the account's change cursor, read before anything is listed so no change is missed), the labels and their counts, then the first page of the inbox. Conversations appear as they load; grey rows are still loading.
- **Paging:** each view lists 50 conversations a page; scrolling near the end loads the next page.
- **Checking for changes:** every minute by default (1, 5 or 15 in Settings), and on `Ctrl+R`, Eloquawk asks Gmail's history for what changed since the cursor and refetches only those conversations. A burst of more than 100 changes refetches only new inbox mail and lets the rest load when you next look at them.
- **Your changes:** archive, trash, star, read state and labels show at once and are sent in the background. If Gmail refuses one, it is put back and a notice says why. `z` undoes the last change.
- **Sending:** a message goes up once and is never retried. If the connection drops part-way, Eloquawk cannot know whether Gmail got it, so it says so and asks you to check Sent before trying again.
- **Errors:** after a network error or a Gmail rate limit, that account backs off (from 5 seconds up to 5 minutes) and its queued work waits, rather than being dropped. When Gmail says a quota is used up, the account waits until Google's stated time, at least 15 minutes, unless you refresh by hand. The banner names the problem and the data on screen stays.
- **History too old:** if Gmail no longer has history back to the cursor (after a long time offline), the account starts over with a first sync.

## Which mail notifies

A desktop notification (org.freedesktop.Notifications) is shown for a conversation when, during a regular check:

- a new message arrives in it that is **unread** and in the **Inbox**, and
- it is not your own (Sent), a draft or spam, and
- Gmail did not file it under **Promotions**, **Social**, **Updates** or **Forums**.

This matches what Gmail itself notifies about with inbox categories on. Mail that is already read by the time Eloquawk refetches it, or has left the inbox, does not notify. Nothing notifies during an account's first sync, so starting Eloquawk never floods you. Up to three conversations notify one by one (sender, subject and the start of the message); more at once become a single "N new conversations" note. A conversation notifies again only when a newer message arrives in it. Turn notifications off in **Settings → Notify about new mail**.

## Further reference

- [Google OAuth for desktop apps](https://developers.google.com/identity/protocols/oauth2/native-app)
- [Gmail API scopes](https://developers.google.com/workspace/gmail/api/auth/scopes)
- [Gmail API usage limits](https://developers.google.com/workspace/gmail/api/reference/quota)

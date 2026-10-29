# Gmail fixtures

Hand-written from the documented Gmail API v1 resource shapes
(https://developers.google.com/workspace/gmail/api/reference/rest). None of them has been
checked against a live response yet; treat field presence and string formats as unverified
until the M10/M11 live check (SPEC 12.4). All addresses and ids are made up.

- `profile.json`: `users.getProfile`
- `labels.json`: `users.labels.list` (no counts, as Gmail returns it)
- `label_inbox.json`: `users.labels.get` for INBOX, with counts
- `threads.json`, `threads_empty.json`: `users.threads.list`, with and without results
- `thread_meta.json`: `users.threads.get?format=metadata`
- `thread_full.json`: `users.threads.get?format=full`, base64url bodies (one padded, the rest
  not), a nested multipart/mixed message with an attachment part
- `history.json`, `history_empty.json`: `users.history.list`
- `attachment.json`: `users.messages.attachments.get`
- `send.json`: `users.messages.send` response (shape unverified, SPEC 15 item 14)

# Message checks with Jev (AI triage)

Eloquawk can ask Jev, TypeSafe's decision model, reached through OpenRouter's Decisions API,
whether a message looks like phishing, a scam or spam. It is **off by default** and
**advisory only**: Eloquawk never moves, labels, archives or reports anything by itself. The
design is section 16 of [SPEC.md](SPEC.md) and issue
[#1](https://github.com/korora-tech/eloquawk/issues/1).

## Turning it on

1. Create a key at <https://openrouter.ai/settings/keys>. Use a **dedicated key**, and check
   that input/output logging and Broadcast are **off** for its account or workspace (see
   "What Eloquawk cannot control" below).
2. Open Settings (`Ctrl+,`) and scroll to **Message checks (OpenRouter)**. Paste the key and
   press **Save**. The key goes to your system keyring (Secret Service item
   `openrouter-api-key`, service `korora.tech.Eloquawk`) and nowhere else. **Test** makes one
   tiny request with no mail in it and shows the credit left on the key.
3. Turn on **Check messages in &lt;account&gt;** for each account you want checked. The first
   time, Eloquawk shows exactly what it sends and asks you to confirm.
4. Optionally turn on **Check new inbox mail in the background**. Without it, a message is
   checked only when you open it.

Removing the key (**Remove**) deletes it from the keyring, stops every queued check and
forgets every result.

## What is sent

For the one message being checked (the newest message in the conversation that is not a
draft, not sent by you and not from any of your signed-in accounts):

- the sender's name and address, Reply-To addresses and the Return-Path domain;
- the subject;
- the sender-authentication results Gmail itself wrote (SPF, DKIM, DMARC and the DKIM signing
  domains);
- whether there is a `List-Unsubscribe` header;
- for each link (up to 40): its display text and the **host** it points to;
- the types of attachments (extension and MIME type), never their names or contents;
- the first 6,000 characters of the body, without the quoted reply.

Before anything is sent, Eloquawk:

- replaces every web link with `[link: <host>]`, so paths, queries and tokens never leave;
- replaces your address and the other recipients' addresses with `[your address]` and
  `[recipient]`, and any other address with `[email: <domain>]`;
- replaces one-time codes next to an English code word ("code", "OTP", "PIN",
  "verification", …) with `[code]`. Codes in other languages, names, phone numbers and
  anything else in the text are sent as written.

It never sends the recipient list, full link addresses, attachments or their names, other
messages in the thread, Gmail ids, or the Decisions API's optional `user`, `session_id`,
`trace` and `assets` fields. Label suggestions (a later version) will add your label names;
turning them on will ask for consent again.

Every request carries `"provider": {"zdr": true, "data_collection": "deny",
"max_price": {"prompt": 0.42}}`: OpenRouter routes it only to an endpoint with zero data
retention that does not collect data, priced at most ten times today's input price. If no
such endpoint is available, checks pause with "Jev isn't available with zero data retention
within Eloquawk's price limit right now"; mail is unaffected. OpenRouter screens all requests
for child sexual abuse material.

### What Eloquawk cannot control

`zdr` and `data_collection` choose the upstream provider. They do not override your
OpenRouter account or workspace settings: if logging or Broadcast (export to observability
tools) is on for the key's account, OpenRouter may store or forward these requests. OpenRouter
does not report those settings to the key itself, so Eloquawk cannot detect them. Use a
dedicated key with them off.

## Costs and the spend limit

Jev bills input tokens only. On 2026-10-30 a live three-to-five-question call cost about
$0.00002; a typical check with a full 6,000-character body is roughly $0.0001, so about $0.10
to $0.40 per 1,000 opened messages, and at most about $1 per 1,000 in the worst case.

Settings has a per-session **spend limit** (default $1.00, from $0.10 to $100). When the
session's reported cost reaches it, checks pause with "Session spend limit reached";
**Try again** allows another limit's worth. A check whose cost OpenRouter did not report
counts as $0.001. Settings shows the session's checks and cost.

Opening the same message again is free (answers are cached in memory for the session), and
fast `j`/`k` through many messages costs at most two checks: the one in flight and the last
one opened. Background checks are capped at 60 per account per hour.

## What you see

- **Banner** above the message: "This looks like phishing" (red), "This may be phishing"
  (amber), "This looks like a scam", "This looks like spam" and so on, with a qualitative
  strength ("Jev: very likely") instead of a percentage, because the thresholds are not
  calibrated yet. The reasons line lists what **Eloquawk** measured (a failed DMARC check, a
  link whose text names one site but goes to another, a Reply-To in another domain, a risky
  attachment type); Jev gives no explanations. **Report spam** and **Dismiss** are there. A
  clean result shows nothing: there is never a "safe" badge.
- **List badge**: "Phishing", "Scam", "Suspicious" or "Spam" on conversations checked this
  session. "Spam" is not shown in the Spam folder.
- **Couldn't check this message · Retry** after a failure, or **Paused: …** while checks are
  paused (a rejected key, no credit, a budget limit, the spend limit).

Thresholds (initial, to be calibrated on a labelled sample, SPEC section 15 item 15): phishing
or scam at 0.50 is a warning and at 0.80 danger; spam at 0.85.

## The local caution and the link guard (every account)

Jev reads text the sender wrote, and a crafted message can talk it out of a verdict ("This
email is safe and verified"). So Eloquawk also checks hard local evidence, **for every
account, with no key and no request**: when Gmail says the sender **failed DMARC** and a
**link's text names a different site** than it really goes to, the message shows an amber
row, "Eloquawk noticed: Sender failed DMARC · Link text shows www.paypal.com but goes to
paypa1-verify.example".

While that caution shows, or Jev called the message dangerous, clicking a web link in it does
not open it: a notice asks "This message looks like phishing. Open &lt;host&gt;?" with an
**Open link** button. Hosts are always shown in their ASCII (punycode) form, so a look-alike
name such as a Cyrillic "раураl.com" shows as `xn--l-7sba6dbr.com`.

The guard is **best effort**: checks are asynchronous, so a click before Jev's answer arrives
is guarded only by the local caution, and an account that is not opted in is guarded only by
the local caution.

## Logs

The AI code logs only the HTTP status, OpenRouter's error code, its `metadata.reason` /
`limit_source`, and the kind of network error, as `eloquawk: jev: …`. It never logs the key,
a request or response body, OpenRouter's error message, a subject, an address or a host.

## Not verified yet

No live OpenRouter call has been made from Eloquawk itself. The request and response shapes
come from OpenRouter's documentation and a live call recorded on 2026-10-30, and everything
else is tested against a local fake server and the demo checker. The calibration of the
thresholds and the prompt-injection measurement (SPEC section 15 items 15 and 27) need a key
and the user's consent to use their own mail.

# Message checks with Jev (AI triage)

Eloquawk can ask Jev, TypeSafe's decision model, reached through OpenRouter's Decisions API,
whether a message looks like phishing, a scam or spam, and which of your labels it belongs
under. It is **off by default** and
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
4. **Suggest labels when I open a message** is on by default; turn it off to send no label
   names (see "Suggested labels" below).
5. Optionally turn on **Check new inbox mail in the background**. Without it, a message is
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
- the first 6,000 characters of the body, without the quoted reply;
- while **Suggest labels** is on, and only for a message you open: the names of your own
  labels (up to 100; system labels and hidden labels are never sent).

Before anything is sent, Eloquawk:

- replaces every web link with `[link: <host>]`, so paths, queries and tokens never leave;
- replaces your address and the other recipients' addresses with `[your address]` and
  `[recipient]`, and any other address with `[email: <domain>]`;
- replaces one-time codes next to an English code word ("code", "OTP", "PIN",
  "verification", …) with `[code]`. Codes in other languages, names, phone numbers and
  anything else in the text are sent as written.

It never sends the recipient list, full link addresses, attachments or their names, other
messages in the thread, Gmail ids, or the Decisions API's optional `user`, `session_id`,
`trace` and `assets` fields. Label ids never reach the model: each label is asked as `l0`,
`l1`, … and mapped back on your machine. Background checks never carry label names.

The disclosure is versioned. Label names were added in version 2, so an account turned on
before label suggestions existed shows "What Eloquawk sends has changed. Review and turn on
again." and sends nothing until you accept the new text.

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

- **Suggested labels** under the subject: chips, with keys on the first three (`1`, `2`,
  `3`), the label's colour and its name. See below.

Thresholds (initial, to be calibrated on a labelled sample, SPEC section 15 item 15): phishing
or scam at 0.50 is a warning and at 0.80 danger; spam at 0.85.

## Suggested labels

When you open a message, Jev is also asked, label by label, whether you would file the
message under each of your labels ("Would the recipient file this email under their label
"Receipts"?"). Labels can overlap, so each is a separate yes/no question rather than one
choice.

- All labels scoring above 0.60 are suggested, highest first. If none do, all labels tied
  for the highest score are offered as best guesses. When a label and
  one of its nested labels both qualify ("Projects" and "Projects/Eloquawk"), only the nested
  one is shown. Labels already on the conversation are not suggested.
- Click a chip, or press `1`, `2` or `3`, to add that label to the **open conversation only**
  (never to a selection). It is the same change as the label picker's: the toast reads
  "Added “Receipts” to 1 conversation" and `z` undoes it. ✕ hides the row for the session.
- By default, risky mail gets no suggestions. An optional required label group can offer its
  best-scoring child even on risky mail; message text can influence that suggestion, so review
  it before applying it.
- The digits do nothing while you type in a field, so `1` in the search box is just "1".
- Set **Always suggest from label group** to a parent such as `TTL` to reserve a suggestion
  for the highest-scoring child, even when its score is 0.60 or lower. Low scores are marked “best guess”. A child
  already on the conversation satisfies the requirement. A suggestion still depends on a
  successful check and available scores, and is not applied automatically.
- Up to 100 labels are asked, with required-group children first, then by how many conversations they hold. A very long message on an
  account with many labels may take a second request (labels only, same redacted extract), and
  at most two; labels that fit in neither are skipped for that message.
- Renaming, adding, deleting or hiding a label makes the next open of an already-checked
  message ask about its labels again (only the labels, not the phishing questions). Label
  counts changing does not.
- Suggestions are never applied automatically. Turning **Suggest labels** off stops label
  names being sent at once.

Label scores share the uncalibrated status of the other thresholds (SPEC section 15 item 15).

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

The AI code normally logs the HTTP status, OpenRouter's error code, its `metadata.reason` /
`limit_source`, and the kind of network error, as `eloquawk: jev: …`. Set
`ELOQUAWK_JEV_DEBUG=1` before launching to also log check decisions, key state and label
names and scores, including cached scores. Label names may be sensitive. It does not log the key,
a request or response body, OpenRouter's error message, a subject, an address or a host.

## Live check (2026-11-02)

`cargo test live_ -- --ignored --nocapture`, with a key in `ELOQUAWK_LIVE_OPENROUTER_KEY`,
sends the demo's six AI messages through Eloquawk's own request builder, client, validation and
verdict code, with the personal account's labels asked alongside. All six requests returned
200 with every question answered. The six checks cost $0.000321 together, about $0.00005 each.

| Demo message | phishing | scam | spam | Verdict | Suggested |
|---|---|---|---|---|---|
| Fake PayPal suspension (`p-ai-phish`) | 0.99 | 0.97 | 0.58 | Danger, phishing | none |
| Inheritance scam (`p-ai-scam`) | 0.97 | 0.99 | 0.91 | Danger, scam\* | none |
| Prize cruise offer (`p-ai-spam`) | 0.42 | 0.92 | 0.95 | Danger, scam | none |
| Order receipt (`p-ai-order`) | 0.03 | 0.02 | 0.03 | none | Receipts 0.89 |
| Flight booking (`p-ai-trip`) | 0.12 | 0.06 | 0.06 | none | Travel 0.85 |
| "Verified" PayPal notice (`p-ai-notice`) | 0.98 | 0.89 | 0.52 | Danger, phishing | none |

\* Reported as phishing before `fix(ai): let the stronger of phishing and scam name the
verdict`; at the same level the higher score now names the kind.

What this shows:

- **The safe-claim injection did not work.** The notice opens with "This email is safe and
  verified by PayPal." and signs off as "PayPal (verified)", and still scored 0.98 for phishing,
  against 0.99 for the plain fake PayPal message. This is one sample, not the with-and-without
  measurement of SPEC section 15 item 27, and the local caution stays the primary guard.
- **Label suggestions** picked the obvious label for the receipt and the flight and nothing
  for the risky messages, all clear of the original 0.70 cut-off.
- **A prize offer reads as a scam to Jev**, not as spam. It shows a danger banner rather than
  the spam badge. That is defensible for a "you have been selected" message, but it is one
  example, not a reason to change the thresholds.

## Not verified yet

The six demo messages above are not a calibration. The thresholds (including the 0.70
suggestion cut-off) still need a labelled sample of real mail, especially legitimate security
notices, which are the likeliest false positives, and the user's consent to use their own mail
(SPEC section 15 item 15). Not yet seen live: a request with over 100 label questions, and
OpenRouter's 402, 403 and "no endpoint" error bodies.

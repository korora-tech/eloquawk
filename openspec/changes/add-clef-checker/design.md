## Context

The AI worker (`src/ai/worker.rs`) owns one key, one `Decider` and a decision cache. Jev is
reached through `src/ai/openrouter.rs` (agent, errors, key type) with request and response
shapes in `src/ai/jev.rs`. Clef's REST body is the Decisions body without `provider`, and
with `model` set to the selector `clef` or `clef-flash`; its answers use the same
`{"type":"noul","noul":p}` shape, and `usage` carries tokens but no cost.

Facts from Cloudflare's docs (2026-11-03), not yet live-verified:
- `POST https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/clef`
  (or `.../clef-flash`), `Authorization: Bearer <token>`.
- Body: `{"model": "clef"|"clef-flash", "state", "questions"}`, 1 to 64 questions, 64k context.
- Output: `{"model", "answers", "usage": {"input_tokens", "output_tokens"}}`; the REST API
  normally wraps it as `{"result": …, "success", "errors", "messages"}`.
- $0.24 per million input tokens, no output charge; zero data retention.

## Goals / Non-Goals

**Goals:** a second checker selectable in Settings, with the same redaction, consent,
pausing, spend limit and log rules; no change for users who stay on Jev.

**Non-Goals:** images (Clef accepts them; Eloquawk sends none), AI Gateway, using both
checkers at once, per-account checkers, calibrating Clef thresholds.

## Decisions

- **`AiBackend` pref** (`jev` | `clef` | `clef_flash`, default `jev`) in `AiPrefs`, plus
  `cloudflare_account_id: String`. One checker for the whole app keeps consent and the spend
  limit simple. Rejected: per-account checkers (consent matrix, two keys live at once).
- **Consent resets on switch.** Settings clears `ai.accounts` together with the backend
  change, so the existing "consent dropped" path forgets queued checks and results. The
  disclosure version stays 2: the Jev disclosure did not change, and Clef consent can only
  be given after a switch.
- **`ApiKey` carries a `Vendor`** (`OpenRouter` | `Cloudflare`). OpenRouter keys keep their
  `sk-or-` rules; Cloudflare tokens are 32 to 256 characters of `[A-Za-z0-9_-]` with the hint
  `…abcd`. The token is stored as Secret Service item `cloudflare-api-token`, labelled
  "Eloquawk: Cloudflare API token". The worker loads the item matching its backend.
- **Account ID in prefs**, validated as 32 lowercase hex characters before it is put in a
  URL path, so it can never change the host or add path segments. The host stays fixed:
  no setting can redirect the token or mail.
- **`AiCommand::Backend { backend, account_id }`** tells the worker about a change. The
  worker drops every job, clears the cache, bumps the epoch, lifts any pause, forgets the
  rejected flag and reloads the key for the new vendor.
- **`src/ai/cloudflare.rs`** builds the Clef body from a `DecisionsRequest` (its `state` and
  `questions`), unwraps `result` when present, fills `usage.cost` from `input_tokens`, and
  classifies Cloudflare errors into the existing `AiError`. `HttpDecider` dispatches on the
  backend. Test skips the credit read (Workers AI has none) and asks the synthetic question.
- **Error mapping (Cloudflare):** 3xx and 404 → Unavailable; 401 and the token-format codes
  (6003, 6111, 9106, 9109, 10000) → KeyRejected; 403 → Forbidden; 413 → TooLarge; 429 whose
  message names a daily or free allocation → Credits, other 429 and 5xx → Busy; anything
  else → Invalid. Logs: status and `errors[].code` only, never `message`.
- **Texts** come from `AiBackend` (`model_name`, `vendor_name`, URLs); `AiError::text(backend)`
  replaces the OpenRouter-only `user_text`.

## Risks / Trade-offs

- [Envelope unverified] → accept both wrapped and bare bodies; the ignored live test
  `live_clef_checks_the_demo_threads` exercises the real endpoint.
- [Clef-flash price unpublished] → count it at Clef's $0.24/M, which can only overstate
  spend.
- [Thresholds tuned on Jev] → same thresholds; calibration stays an open item.
- [Free-allocation 429 wording unverified] → if not matched it is retried as Busy and then
  fails the one check, which is safe.

## Why

Cloudflare released Clef and Clef-flash, open-weight decision models on Workers AI that take
the same `state` and typed `questions` as Jev and answer `noul` questions the same way
(<https://blog.cloudflare.com/clef-decision-models/>). Offering them as an alternative to Jev
through OpenRouter gives users a second processor to choose from, with zero data retention by
default, a larger context window and lower reported latency.

## What Changes

- Settings' message-checks card gains a checker choice: "Jev (OpenRouter)" (the default and
  today's behaviour), "Clef (Cloudflare)" and "Clef-flash (Cloudflare)".
- With a Clef checker, checks go to Cloudflare's Workers AI REST API at
  `api.cloudflare.com/client/v4/accounts/<account id>/ai/run/@cf/cloudflare/clef` (or
  `clef-flash`), authenticated with a Cloudflare API token kept in the Secret Service, plus a
  Cloudflare account ID kept in the settings file (it is not a secret).
- **Data leaving the machine**: the same redacted extract and label names as today, sent to
  Cloudflare instead of OpenRouter when a Clef checker is chosen. Never to both.
- Switching checker turns message checks off for every account until the user reviews the
  new disclosure and turns them on again, and forgets every result of the session.
- User-facing texts that name the processor or the model (card intro, disclosure, privacy
  links, key status, errors, Test result, reader banner wording, guidance caption) name the
  chosen one.
- Cost for Clef is computed from reported input tokens at $0.24 per million (Clef does not
  charge for output) and counts towards the existing session spend limit.
- No new dependency; `cargo tree -i tokio` stays empty. Nothing new is written to disk except
  the checker choice and the account ID in the settings file.

## Capabilities

### Modified Capabilities
- `message-checks`: checker choice, Cloudflare credentials, routing, errors and texts.
- `label-suggestions`: the guidance caption names the chosen processor.
- `settings-and-theme`: the card is titled "Message checks".
- `demo-mode`: demo mode contacts neither OpenRouter nor Cloudflare.

## Impact

- `src/ai/` (new `cloudflare.rs`, worker, request, validation), `src/config.rs`,
  `src/secrets.rs`, `src/store.rs`, `src/ui/settings.rs`, `src/ui/triage.rs`.
- docs/ai-triage.md, docs/SPEC.md section 16, README.
- Thresholds were tuned on Jev only; Clef uses the same ones until calibrated on real mail.

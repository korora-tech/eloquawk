## 1. Cloudflare client

- [x] 1.1 `feat(ai)`: `Vendor` on `ApiKey`, Cloudflare token rules and Secret Service item
- [x] 1.2 `feat(ai)`: `cloudflare.rs` — Clef body, envelope, cost, error classification, tests

## 2. Checker choice

- [x] 2.1 `feat(ai)`: `AiBackend` and account ID prefs; worker `Backend` command, dispatch and
      checker-named error texts; worker tests against a fake Cloudflare server
- [x] 2.2 `feat(settings)`: checker picker, account ID field, checker-named texts; banner
      wording from the checker

## 3. Docs and specs

- [x] 3.1 `docs`: ai-triage.md, SPEC.md section 16, README
- [x] 3.2 `docs(openspec)`: archive this change into the specs

## 4. Checks

- [x] 4.1 cargo test, clippy, `cargo tree -i tokio` empty
- [ ] 4.2 ignored live test `live_clef_checks_the_demo_threads` (needs a Cloudflare token)

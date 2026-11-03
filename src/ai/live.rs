//! The live checks against OpenRouter and Cloudflare (SPEC 16.15): the demo's AI threads go
//! through the real request builder, client, validation and verdict code. Ignored by default
//! and skipped without `ELOQUAWK_LIVE_OPENROUTER_KEY`, or `ELOQUAWK_LIVE_CLOUDFLARE_TOKEN`
//! and `ELOQUAWK_LIVE_CLOUDFLARE_ACCOUNT` (optionally `ELOQUAWK_LIVE_CLEF=clef-flash`), so
//! `cargo test` never touches the network; run them with
//! `cargo test live_ -- --ignored --nocapture`. Prints only ids, scores and costs.

use super::AiBackend;
use super::cloudflare;
use super::jev::{
    DecisionsRequest, DecisionsResponse, THRESHOLDS, select_suggestions, validate, verdict_from,
};
use super::openrouter::{AiEndpoints, AiError, ApiKey, agent, get_key, post_decisions};
use super::prompt::{AiInput, Redact, fit, label_candidates, target};
use crate::demo_data::{self, PERSONAL, WORK};

const KEY_VAR: &str = "ELOQUAWK_LIVE_OPENROUTER_KEY";
const TOKEN_VAR: &str = "ELOQUAWK_LIVE_CLOUDFLARE_TOKEN";
const ACCOUNT_VAR: &str = "ELOQUAWK_LIVE_CLOUDFLARE_ACCOUNT";
const CLEF_VAR: &str = "ELOQUAWK_LIVE_CLEF";

const THREADS: [&str; 6] = [
    "p-ai-phish",
    "p-ai-scam",
    "p-ai-spam",
    "p-ai-order",
    "p-ai-trip",
    "p-ai-notice",
];

#[test]
#[ignore = "calls OpenRouter; needs ELOQUAWK_LIVE_OPENROUTER_KEY"]
fn live_jev_checks_the_demo_threads() {
    let raw = std::env::var(KEY_VAR).unwrap_or_default();
    if raw.is_empty() {
        eprintln!("{KEY_VAR} is not set; skipping");
        return;
    }
    let key = ApiKey::parse(&raw).expect("a valid key");
    let (agent, endpoints) = (agent(), AiEndpoints::production());
    let credit = get_key(&agent, &endpoints, &key).expect("the key endpoint answers");
    eprintln!("key ok, limit_remaining={credit:?}");
    check_demo_threads(|req| {
        let body = post_decisions(&agent, &endpoints, &key, &req.to_bytes())?;
        serde_json::from_slice(&body).map_err(|_| AiError::Decode)
    });
}

#[test]
#[ignore = "calls Cloudflare; needs ELOQUAWK_LIVE_CLOUDFLARE_TOKEN and _ACCOUNT"]
fn live_clef_checks_the_demo_threads() {
    let (raw, account) = (
        std::env::var(TOKEN_VAR).unwrap_or_default(),
        std::env::var(ACCOUNT_VAR).unwrap_or_default(),
    );
    if raw.is_empty() || account.is_empty() {
        eprintln!("{TOKEN_VAR} or {ACCOUNT_VAR} is not set; skipping");
        return;
    }
    let backend = match std::env::var(CLEF_VAR).as_deref() {
        Ok("clef-flash") => AiBackend::ClefFlash,
        _ => AiBackend::Clef,
    };
    let key = ApiKey::parse_cloudflare(&raw).expect("a valid token");
    assert!(cloudflare::valid_account_id(&account), "a valid account ID");
    let (agent, endpoints) = (agent(), AiEndpoints::production());
    eprintln!("checking with {}", backend.model_name());
    check_demo_threads(|req| {
        let body = cloudflare::body(req, backend).expect("a Clef body");
        let resp = cloudflare::post_run(&agent, &endpoints, &account, backend, &key, &body)?;
        cloudflare::parse(&resp)
    });
}

/// Checks the demo's AI threads through `decide`, printing scores, suggestions and cost.
fn check_demo_threads(decide: impl Fn(&DecisionsRequest) -> Result<DecisionsResponse, AiError>) {
    let own = vec![PERSONAL.to_string(), WORK.to_string()];
    let labels = label_candidates(&demo_data::label_defs(PERSONAL));
    let mailbox = demo_data::generate(jiff::Timestamp::now());
    let mut total = 0.0;
    for id in THREADS {
        let detail = &mailbox.threads.iter().find(|t| t.0.key.id == id).unwrap().1;
        let m = target(detail, &own).unwrap();
        let plan = fit(
            AiInput::from_message(m, &Redact::for_message(m, &own)),
            true,
            &labels,
        );
        let mut probs = Vec::new();
        let mut requests = vec![plan.first_request(true)];
        requests.extend(plan.second_request());
        let mut verdict = None;
        for (n, req) in requests.iter().enumerate() {
            let asked: Vec<&str> = req.questions.keys().map(String::as_str).collect();
            let resp = decide(req).unwrap_or_else(|e| panic!("{id}: request {n} failed: {e:?}"));
            let answers = validate(&resp, &asked).expect("every question answered");
            total += answers.cost.unwrap_or(0.0);
            let p = |q: &str| answers.probs.get(q).copied().unwrap_or(0.0);
            if n == 0 {
                verdict = Some(verdict_from(
                    p("phishing"),
                    p("scam"),
                    p("spam"),
                    &THRESHOLDS,
                ));
            }
            for (q, p) in &answers.probs {
                if let Some(i) = q.strip_prefix('l').and_then(|i| i.parse().ok())
                    && let Some(label) = plan.label_id(i)
                {
                    probs.push((label.to_string(), *p));
                }
            }
        }
        let v = verdict.unwrap();
        let suggested = select_suggestions(&probs, &labels, &m.label_ids);
        eprintln!(
            "{id}: {:?}/{:?} phishing={:.2} scam={:.2} spam={:.2} requests={} suggest={:?}",
            v.level,
            v.kind,
            v.p_phishing,
            v.p_scam,
            v.p_spam,
            requests.len(),
            suggested
                .iter()
                .map(|s| format!("{} {:.2}", s.name, s.p))
                .collect::<Vec<_>>()
        );
    }
    eprintln!("total cost ${total:.6}");
}

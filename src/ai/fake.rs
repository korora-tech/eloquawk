//! The offline checker demo mode uses (SPEC 16.13): deterministic answers computed from the
//! request's own `state`, so the real request builder is exercised end to end without a key
//! or a network. Two subject markers reach the failure and pause paths.

use std::time::Duration;

use super::jev::{Answer, DecisionsRequest, DecisionsResponse, Usage};
use super::openrouter::AiError;
use crate::ai::JEV_MODEL;

/// How long a fake decision "takes".
pub const FAKE_LATENCY: Duration = Duration::from_millis(300);

pub struct FakeDecider {
    pub delay: Duration,
}

impl Default for FakeDecider {
    fn default() -> Self {
        Self {
            delay: FAKE_LATENCY,
        }
    }
}

fn s<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

/// The quoted name in a label question's instructions.
fn label_name(instructions: &str) -> Option<&str> {
    let start = instructions.find('"')? + 1;
    let end = instructions.rfind('"')?;
    (end > start).then(|| &instructions[start..end])
}

impl FakeDecider {
    pub fn decide(&self, req: &DecisionsRequest) -> Result<DecisionsResponse, AiError> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let st = &req.state;
        let subject = s(st, "subject");
        if subject.contains("[jev-fail]") {
            return Err(AiError::Busy {
                retry_after: Some(Duration::ZERO),
            });
        }
        if subject.contains("[jev-pause]") {
            return Err(AiError::Credits);
        }
        let body = s(st, "body_text").to_lowercase();
        let dmarc_fail = st
            .get("sender_authentication")
            .map(|a| s(a, "dmarc") == "fail")
            .unwrap_or(false);
        let mismatch = st
            .get("links")
            .and_then(|l| l.as_array())
            .is_some_and(|links| {
                links.iter().any(|l| {
                    l.get("shown_address_differs_from_destination")
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false)
                })
            });
        let phishing = if body.contains("this email is safe") {
            0.04
        } else {
            match (dmarc_fail, mismatch) {
                (true, true) => 0.97,
                (true, false) | (false, true) => 0.62,
                (false, false) => 0.03,
            }
        };
        let scam = if ["inheritance", "gift card", "wire the fee", "bitcoin"]
            .iter()
            .any(|w| body.contains(w))
        {
            0.91
        } else {
            0.02
        };
        let unsubscribe = st
            .get("has_unsubscribe_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let spam = if !unsubscribe && (subject.contains("WIN") || subject.contains("Limited offer"))
        {
            0.90
        } else {
            0.05
        };
        let haystack = format!(
            "{} {} {}",
            subject.to_lowercase(),
            body,
            s(st, "sender_address")
                .rsplit('@')
                .next()
                .unwrap_or_default()
                .to_lowercase()
        );
        let answers = req
            .questions
            .iter()
            .map(|(id, q)| {
                let p = match id.as_str() {
                    "phishing" => phishing,
                    "scam" => scam,
                    "spam" => spam,
                    _ if id.starts_with('l') => {
                        let name = label_name(&q.instructions).unwrap_or_default();
                        let leaf = name.rsplit('/').next().unwrap_or(name).to_lowercase();
                        if !leaf.is_empty() && haystack.contains(&leaf) {
                            0.92
                        } else if name == "Receipts"
                            && ["receipt", "order", "invoice"]
                                .iter()
                                .any(|w| body.contains(w))
                        {
                            0.88
                        } else {
                            0.08
                        }
                    }
                    _ => 0.5,
                };
                (
                    id.clone(),
                    Answer {
                        kind: "noul".into(),
                        noul: Some(p),
                    },
                )
            })
            .collect();
        Ok(DecisionsResponse {
            model: format!("{JEV_MODEL}-demo"),
            answers,
            usage: Some(Usage {
                input_tokens: 0,
                output_tokens: 0,
                cost: Some(0.0),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::jev::{Kind, Level, THRESHOLDS, validate, verdict_from};
    use crate::ai::prompt::{AiInput, Redact, TRIAGE_IDS, fit, target, triage_request};
    use crate::demo_data::{self, PERSONAL, WORK};

    fn verdict_of(id: &str) -> Result<(Level, Kind), AiError> {
        let m = demo_data::generate("2026-10-29T12:00:00Z".parse().unwrap());
        let detail = m.threads.into_iter().find(|t| t.0.key.id == id).unwrap().1;
        let own = vec![PERSONAL.to_string(), WORK.to_string()];
        let msg = target(&detail, &own).unwrap();
        let input = fit(
            AiInput::from_message(msg, &Redact::for_message(msg, &own)),
            true,
            &[],
        )
        .input;
        let fake = FakeDecider {
            delay: Duration::ZERO,
        };
        let resp = fake.decide(&triage_request(&input))?;
        let a = validate(&resp, &TRIAGE_IDS)?;
        assert_eq!(a.cost, Some(0.0));
        let v = verdict_from(
            a.probs["phishing"],
            a.probs["scam"],
            a.probs["spam"],
            &THRESHOLDS,
        );
        Ok((v.level, v.kind))
    }

    #[test]
    fn demo_messages_get_their_expected_verdicts() {
        assert_eq!(
            verdict_of("p-ai-phish"),
            Ok((Level::Danger, Kind::Phishing))
        );
        assert_eq!(verdict_of("p-ai-scam"), Ok((Level::Danger, Kind::Scam)));
        assert_eq!(verdict_of("p-ai-spam"), Ok((Level::Spam, Kind::Spam)));
        assert_eq!(verdict_of("p-ai-order").map(|v| v.0), Ok(Level::None));
        assert_eq!(verdict_of("p-ai-trip").map(|v| v.0), Ok(Level::None));
        assert_eq!(verdict_of("p-ai-notice").map(|v| v.0), Ok(Level::None));
        assert_eq!(verdict_of("p-news"), Ok((Level::Warning, Kind::Phishing)));
        assert_eq!(verdict_of("p-cafe").map(|v| v.0), Ok(Level::None));
    }

    #[test]
    fn markers_fail_and_pause() {
        assert_eq!(
            verdict_of("p-ai-fail"),
            Err(AiError::Busy {
                retry_after: Some(Duration::ZERO)
            })
        );
        assert_eq!(verdict_of("p-ai-pause"), Err(AiError::Credits));
    }

    /// The suggestions the offline checker leads to through the real builder, before the
    /// store's rule that risky mail gets none.
    fn suggestions_of(id: &str) -> Vec<String> {
        use crate::ai::jev::select_suggestions;
        use crate::ai::prompt::{label_candidates, label_question_id};
        let m = demo_data::generate("2026-10-29T12:00:00Z".parse().unwrap());
        let detail = m.threads.into_iter().find(|t| t.0.key.id == id).unwrap().1;
        let own = vec![PERSONAL.to_string(), WORK.to_string()];
        let msg = target(&detail, &own).unwrap();
        let candidates = label_candidates(&demo_data::label_defs(PERSONAL));
        let plan = fit(
            AiInput::from_message(msg, &Redact::for_message(msg, &own)),
            true,
            &candidates,
        );
        let fake = FakeDecider {
            delay: Duration::ZERO,
        };
        let resp = fake.decide(&plan.first_request(true)).unwrap();
        let probs: Vec<(String, f32)> = (0..plan.labels_asked())
            .filter_map(|i| {
                let a = resp.answers.get(&label_question_id(i))?;
                Some((plan.label_id(i)?.to_string(), a.noul? as f32))
            })
            .collect();
        select_suggestions(&probs, &candidates, &detail.label_ids)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn demo_messages_get_their_expected_suggestions() {
        assert_eq!(suggestions_of("p-ai-order"), ["Receipts"]);
        assert_eq!(suggestions_of("p-ai-trip"), ["Travel"]);
        assert!(suggestions_of("p-cafe").is_empty());
        // Already filed: the render-time filter leaves nothing to suggest.
        assert!(suggestions_of("p-receipt").is_empty());
        // The crafted notice steers the checker towards Receipts; the store hides it.
        assert_eq!(suggestions_of("p-ai-notice"), ["Receipts"]);
    }
}

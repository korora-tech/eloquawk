//! The Decisions API's request and response shapes, response validation and the mapping from
//! Jev's scores to a verdict (SPEC 16.6.6, 16.7). Pure.
//!
//! Live facts checked on 2026-10-30 (SPEC 15): a 200 body is
//! `{"model":"typesafe/jev-1.13-20260917","answers":{"<id>":{"type":"noul","noul":0.97}},
//! "usage":{"input_tokens","output_tokens","cost"},"id","provider"}`, and question types are
//! `noul`, `choice`, `score` and `box`. Eloquawk asks only `noul` questions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::openrouter::AiError;
use super::{JEV_MODEL, LabelCandidate};

/// `provider.max_price.prompt`: ten times the live input price, in dollars per million tokens.
pub const MAX_PROMPT_PRICE: f64 = 0.42;

/// `{"true": …, "false": …}`, required for every `noul`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Criteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// One question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    #[serde(rename = "type")]
    pub kind: String,
    pub instructions: String,
    pub criteria: Criteria,
}

impl Question {
    pub fn noul(instructions: impl Into<String>, yes: &str, no: &str) -> Self {
        Self {
            kind: "noul".into(),
            instructions: instructions.into(),
            criteria: Criteria {
                yes: yes.into(),
                no: no.into(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaxPrice {
    pub prompt: f64,
}

/// Routing constraints sent with every request (SPEC 16.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    pub zdr: bool,
    pub data_collection: String,
    pub max_price: MaxPrice,
}

impl Provider {
    pub fn eloquawk() -> Self {
        Self {
            zdr: true,
            data_collection: "deny".into(),
            max_price: MaxPrice {
                prompt: MAX_PROMPT_PRICE,
            },
        }
    }
}

/// The request body. `user`, `session_id`, `trace` and `assets` are never sent. Not `Debug`:
/// its state is mail.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionsRequest {
    pub model: String,
    pub state: serde_json::Value,
    pub questions: BTreeMap<String, Question>,
    pub provider: Provider,
}

impl DecisionsRequest {
    pub fn new(state: serde_json::Value, questions: BTreeMap<String, Question>) -> Self {
        Self {
            model: JEV_MODEL.into(),
            state,
            questions,
            provider: Provider::eloquawk(),
        }
    }

    /// The serialised body, whose length the size budget counts.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// One answer. Only `noul` is used; other fields of other types are ignored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionsResponse {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// The validated answers of one request.
#[derive(Clone, Debug, PartialEq)]
pub struct Answers {
    /// Question id → probability of "true".
    pub probs: BTreeMap<String, f32>,
    /// `usage.cost` in USD, when reported.
    pub cost: Option<f64>,
}

/// Parses and validates a 200 body (SPEC 16.7). `Decode` on any failure. The worker
/// deserialises through its decider and calls [`validate`]; this is the same path in one call.
#[cfg(test)]
pub fn parse_response(bytes: &[u8], asked: &[&str]) -> Result<Answers, AiError> {
    let resp: DecisionsResponse = serde_json::from_slice(bytes).map_err(|_| AiError::Decode)?;
    validate(&resp, asked)
}

/// Whether the unexpected-model line was already logged this session.
static MODEL_LOGGED: AtomicBool = AtomicBool::new(false);

/// Checks that every asked id has a `noul` answer, a finite number in [0, 1]. Extra answers are
/// ignored, and a missing `usage.cost` is reported as `None`.
pub fn validate(resp: &DecisionsResponse, asked: &[&str]) -> Result<Answers, AiError> {
    let mut probs = BTreeMap::new();
    for id in asked {
        let a = resp.answers.get(*id).ok_or(AiError::Decode)?;
        let p = match (a.kind.as_str(), a.noul) {
            ("noul", Some(p)) if p.is_finite() && (0.0..=1.0).contains(&p) => p,
            _ => return Err(AiError::Decode),
        };
        probs.insert(id.to_string(), p as f32);
    }
    if !resp.model.starts_with(JEV_MODEL) && !MODEL_LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("eloquawk: jev: unexpected model");
    }
    Ok(Answers {
        probs,
        cost: resp.usage.as_ref().and_then(|u| u.cost),
    })
}

/// How bad a message looks. Ordered: the verdict's level is the highest one reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    None,
    Spam,
    Warning,
    Danger,
}

/// What drives the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Phishing,
    Scam,
    Spam,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    pub level: Level,
    pub kind: Kind,
    pub p_phishing: f32,
    pub p_scam: f32,
    pub p_spam: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub warning: f32,
    pub danger: f32,
    pub spam: f32,
}

/// Initial values, to be calibrated (SPEC 15 item 15).
pub const THRESHOLDS: Thresholds = Thresholds {
    warning: 0.50,
    danger: 0.80,
    spam: 0.85,
};

/// Maps the three scores to a verdict (SPEC 16.7): phishing and scam give Warning or Danger,
/// the higher score names the kind when both reach the same level (phishing on an exact tie),
/// and spam alone never goes above `Level::Spam`.
pub fn verdict_from(p_phishing: f32, p_scam: f32, p_spam: f32, t: &Thresholds) -> Verdict {
    let level_of = |p: f32| {
        if p >= t.danger {
            Level::Danger
        } else if p >= t.warning {
            Level::Warning
        } else {
            Level::None
        }
    };
    let (phish, scam) = (level_of(p_phishing), level_of(p_scam));
    let phish_leads = phish > scam || (phish == scam && p_phishing >= p_scam);
    let (level, kind) = if phish_leads && phish > Level::None {
        (phish, Kind::Phishing)
    } else if scam > Level::None {
        (scam, Kind::Scam)
    } else if p_spam >= t.spam {
        (Level::Spam, Kind::Spam)
    } else {
        (Level::None, Kind::Phishing)
    };
    Verdict {
        level,
        kind,
        p_phishing,
        p_scam,
        p_spam,
    }
}

/// Labels scoring strictly above this value qualify without a fallback.
pub const SUGGEST_MIN: f32 = 0.60;

/// One suggested label for the open thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub id: String,
    pub name: String,
    pub p: f32,
}

/// Whether `ancestor` is a parent of `name` by `/` nesting ("A" of "A/B").
fn is_ancestor(ancestor: &str, name: &str) -> bool {
    name.len() > ancestor.len() + 1
        && name.starts_with(ancestor)
        && name.as_bytes()[ancestor.len()] == b'/'
}

/// Show all labels above the threshold, or all labels tied for the highest score when
/// none clears it. Suppress ancestors of selected descendants and already-applied labels.
pub fn select_suggestions(
    probs: &[(String, f32)],
    labels: &[LabelCandidate],
    applied: &BTreeSet<String>,
) -> Vec<Suggestion> {
    let eligible: Vec<Suggestion> = probs
        .iter()
        .filter(|(_, p)| p.is_finite() && (0.0..=1.0).contains(p))
        .filter_map(|(id, p)| {
            let label = labels.iter().find(|l| &l.id == id)?;
            Some(Suggestion {
                id: id.clone(),
                name: label.name.clone(),
                p: *p,
            })
        })
        .collect();
    let any_above = eligible.iter().any(|s| s.p > SUGGEST_MIN);
    let max = eligible.iter().map(|s| s.p).max_by(f32::total_cmp);
    let qualified: Vec<_> = eligible
        .into_iter()
        .filter(|s| {
            if any_above {
                s.p > SUGGEST_MIN
            } else {
                Some(s.p) == max
            }
        })
        .collect();
    let mut out: Vec<_> = qualified
        .iter()
        .filter(|s| !qualified.iter().any(|d| is_ancestor(&s.name, &d.name)))
        .filter(|s| !applied.contains(&s.id))
        .cloned()
        .collect();
    out.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Pick the best scored leaf of a required group, unless one is already applied.
pub fn required_group_suggestion(
    probs: &[(String, f32)],
    labels: &[LabelCandidate],
    applied: &BTreeSet<String>,
    group: &str,
) -> Option<Suggestion> {
    use super::prompt::in_label_group;
    if labels
        .iter()
        .any(|label| applied.contains(&label.id) && in_label_group(&label.name, group))
    {
        return None;
    }
    let eligible: Vec<_> = labels
        .iter()
        .filter(|label| in_label_group(&label.name, group))
        .collect();
    probs
        .iter()
        .filter(|(_, p)| p.is_finite() && (0.0..=1.0).contains(p))
        .filter_map(|(id, p)| {
            let label = eligible.iter().find(|label| label.id == *id)?;
            if eligible
                .iter()
                .any(|child| is_ancestor(&label.name, &child.name))
            {
                return None;
            }
            Some(Suggestion {
                id: id.clone(),
                name: label.name.clone(),
                p: *p,
            })
        })
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.name.cmp(&a.name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("fixtures/response_live.json");
    const CHOICE: &str = include_str!("fixtures/response_choice.json");

    #[test]
    fn parses_the_live_response_shape() {
        let a = parse_response(LIVE.as_bytes(), &["phishing", "scam", "spam"]).unwrap();
        assert_eq!(a.probs["phishing"], 0.97);
        assert_eq!(a.probs["scam"], 0.12);
        assert_eq!(a.cost, Some(0.000018816));
        let resp: DecisionsResponse = serde_json::from_str(LIVE).unwrap();
        assert_eq!(resp.model, "typesafe/jev-1.13-20260917");
        assert!(resp.model.starts_with(JEV_MODEL));
    }

    #[test]
    fn missing_cost_is_none_and_extra_ids_are_ignored() {
        let body = r#"{"model":"typesafe/jev-1.13","answers":{"phishing":{"type":"noul","noul":0.2},
            "extra":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let a = parse_response(body.as_bytes(), &["phishing"]).unwrap();
        assert_eq!(a.cost, None);
        assert_eq!(a.probs.len(), 1);
        let no_usage = r#"{"model":"m","answers":{"phishing":{"type":"noul","noul":0}}}"#;
        assert_eq!(
            parse_response(no_usage.as_bytes(), &["phishing"])
                .unwrap()
                .cost,
            None
        );
    }

    #[test]
    fn out_of_range_missing_and_wrong_types_are_rejected() {
        let one = |v: &str| format!(r#"{{"model":"m","answers":{{"phishing":{v}}}}}"#);
        for bad in [
            r#"{"type":"noul","noul":1.2}"#,
            r#"{"type":"noul","noul":-0.1}"#,
            r#"{"type":"noul"}"#,
            r#"{"type":"noul","noul":"0.5"}"#,
            r#"{"type":"score","score":0.5}"#,
        ] {
            assert_eq!(
                parse_response(one(bad).as_bytes(), &["phishing"]),
                Err(AiError::Decode),
                "{bad}"
            );
        }
        assert_eq!(
            parse_response(LIVE.as_bytes(), &["phishing", "l0"]),
            Err(AiError::Decode)
        );
        assert_eq!(
            parse_response(b"not json", &["phishing"]),
            Err(AiError::Decode)
        );
        // A choice answer parses as a response but is not a noul.
        assert_eq!(
            parse_response(CHOICE.as_bytes(), &["kind"]),
            Err(AiError::Decode)
        );
    }

    fn level(p: f32, s: f32, sp: f32) -> (Level, Kind) {
        let v = verdict_from(p, s, sp, &THRESHOLDS);
        (v.level, v.kind)
    }

    #[test]
    fn verdict_table_boundaries() {
        assert_eq!(level(0.49, 0.0, 0.0).0, Level::None);
        assert_eq!(level(0.50, 0.0, 0.0), (Level::Warning, Kind::Phishing));
        assert_eq!(level(0.79, 0.0, 0.0), (Level::Warning, Kind::Phishing));
        assert_eq!(level(0.80, 0.0, 0.0), (Level::Danger, Kind::Phishing));
        assert_eq!(level(0.0, 0.49, 0.0).0, Level::None);
        assert_eq!(level(0.0, 0.50, 0.0), (Level::Warning, Kind::Scam));
        assert_eq!(level(0.0, 0.80, 0.0), (Level::Danger, Kind::Scam));
        assert_eq!(level(0.0, 0.0, 0.84).0, Level::None);
        assert_eq!(level(0.0, 0.0, 0.85), (Level::Spam, Kind::Spam));
        assert_eq!(level(0.0, 0.0, 1.0), (Level::Spam, Kind::Spam));
        // The higher level wins; at the same level the higher score names it, and an exact
        // tie goes to phishing. The live inheritance scam scored phishing 0.97, scam 0.99.
        assert_eq!(level(0.9, 0.9, 0.0), (Level::Danger, Kind::Phishing));
        assert_eq!(level(0.6, 0.6, 0.0), (Level::Warning, Kind::Phishing));
        assert_eq!(level(0.97, 0.99, 0.0), (Level::Danger, Kind::Scam));
        assert_eq!(level(0.99, 0.97, 0.0), (Level::Danger, Kind::Phishing));
        assert_eq!(level(0.55, 0.7, 0.0), (Level::Warning, Kind::Scam));
        assert_eq!(level(0.85, 0.7, 0.0), (Level::Danger, Kind::Phishing));
        assert_eq!(level(0.6, 0.9, 0.0), (Level::Danger, Kind::Scam));
        // Spam never outranks a warning.
        assert_eq!(level(0.55, 0.0, 0.99), (Level::Warning, Kind::Phishing));
        assert!(Level::Danger > Level::Warning && Level::Warning > Level::Spam);
    }

    #[test]
    fn requests_carry_the_provider_constraints() {
        let mut q = BTreeMap::new();
        q.insert(
            "t".to_string(),
            Question::noul("Is it?", "It is.", "It is not."),
        );
        let req = DecisionsRequest::new(serde_json::json!("Eloquawk connection test."), q);
        let v: serde_json::Value = serde_json::from_slice(&req.to_bytes()).unwrap();
        assert_eq!(
            v["provider"],
            serde_json::json!({"zdr": true, "data_collection": "deny", "max_price": {"prompt": 0.42}})
        );
        assert_eq!(v["model"], "typesafe/jev-1.13");
        assert_eq!(
            v["questions"]["t"],
            serde_json::json!({"type":"noul","instructions":"Is it?","criteria":{"true":"It is.","false":"It is not."}})
        );
        for absent in ["user", "session_id", "trace", "assets"] {
            assert!(v.get(absent).is_none(), "{absent}");
        }
    }

    fn cand(id: &str, name: &str) -> LabelCandidate {
        LabelCandidate {
            id: id.into(),
            name: name.into(),
        }
    }

    fn names(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn required_group_uses_best_leaf_even_below_threshold_and_respects_applied() {
        let labels = vec![
            cand("root", "TTL"),
            cand("m3", "TTL/M3"),
            cand("y5", "TTL/Y5"),
            cand("other", "Other"),
        ];
        let probs = vec![
            ("root".into(), 0.99),
            ("m3".into(), 0.3),
            ("y5".into(), 0.5),
            ("other".into(), 0.99),
        ];
        assert_eq!(
            required_group_suggestion(&probs, &labels, &BTreeSet::new(), "ttl")
                .unwrap()
                .id,
            "y5"
        );
        assert!(required_group_suggestion(&probs, &labels, &["m3".into()].into(), "TTL").is_none());
        assert!(required_group_suggestion(&[], &labels, &BTreeSet::new(), "TTL").is_none());
        assert!(required_group_suggestion(&probs, &labels, &BTreeSet::new(), "").is_none());
    }

    #[test]
    fn suggestions_show_all_above_threshold_and_exclude_applied() {
        let labels = vec![
            cand("a", "Alpha"),
            cand("b", "Beta"),
            cand("c", "Gamma"),
            cand("d", "Delta"),
            cand("e", "Epsilon"),
        ];
        let probs = vec![
            ("a".to_string(), 0.69),
            ("b".to_string(), 0.70),
            ("c".to_string(), 0.95),
            ("d".to_string(), 0.80),
            ("e".to_string(), 0.80),
            ("gone".to_string(), 0.99),
        ];
        let none = BTreeSet::new();
        let s = select_suggestions(&probs, &labels, &none);
        // All scores above 0.60 are kept; a deleted label is skipped.
        assert_eq!(names(&s), ["Gamma", "Delta", "Epsilon", "Beta", "Alpha"]);
        // Applied labels are hidden.

        let applied: BTreeSet<String> = ["c".to_string()].into();
        let s = select_suggestions(&probs, &labels, &applied);
        assert_eq!(names(&s), ["Delta", "Epsilon", "Beta", "Alpha"]);
    }

    #[test]
    fn nested_ttl_from_report_selects_qualifying_child() {
        let labels = vec![
            cand("root", "TTL"),
            cand("m1", "TTL/M1"),
            cand("m3", "TTL/M3"),
        ];
        let scores = vec![
            ("root".into(), 0.68),
            ("m1".into(), 0.61),
            ("m3".into(), 0.52),
        ];
        assert_eq!(
            names(&select_suggestions(&scores, &labels, &BTreeSet::new())),
            ["TTL/M1"]
        );
    }

    #[test]
    fn fallback_shows_highest_ties_and_does_not_resuggest_after_application() {
        let labels = vec![cand("a", "Alpha"), cand("b", "Beta"), cand("c", "Other")];
        let probs = vec![("a".into(), 0.6), ("b".into(), 0.6), ("c".into(), 0.5)];
        assert_eq!(
            names(&select_suggestions(&probs, &labels, &BTreeSet::new())),
            ["Alpha", "Beta"]
        );
        assert_eq!(
            names(&select_suggestions(&probs, &labels, &["a".into()].into())),
            ["Beta"]
        );
        assert!(select_suggestions(&probs, &labels, &["a".into(), "b".into()].into()).is_empty());
        assert!(select_suggestions(&[], &labels, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn suggestions_drop_an_ancestor_when_a_descendant_qualifies() {
        let labels = vec![
            cand("p", "Projects"),
            cand("pe", "Projects/Eloquawk"),
            cand("pr", "Projectsx"),
            cand("w", "Work"),
        ];
        let probs = vec![
            ("p".to_string(), 0.95),
            ("pe".to_string(), 0.75),
            ("pr".to_string(), 0.9),
            ("w".to_string(), 0.9),
        ];
        let s = select_suggestions(&probs, &labels, &BTreeSet::new());
        assert_eq!(names(&s), ["Projectsx", "Work", "Projects/Eloquawk"]);
        // Without the descendant, the ancestor stays.
        let s = select_suggestions(&probs[..1], &labels, &BTreeSet::new());
        assert_eq!(names(&s), ["Projects"]);
    }
}

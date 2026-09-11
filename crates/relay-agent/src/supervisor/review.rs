use super::discover::{Backend, Discovered};
use super::pool::Pool;
use crate::automation::{Providers, ReviewDepth, ReviewRules, Strategy};
use anyhow::{bail, Result};
use relay_adapters::family::Family;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REVIEW_SYSTEM: &str = "You are a reviewer from a different model family than the agent \
whose work you are reading. You never continue that agent's work and you never touch anything. \
You judge the work against the most recent instruction the human gave, which is quoted to you as \
the current task. Short follow-up questions from the human are context, never the task itself. A session can run for months and change subject many times, so the goal stated \
when the session opened is background only: never treat it as the task in hand and never tell the \
agent to abandon the current task for it. Answer with a single JSON object and nothing else: \
{\"verdict\": \"on_track\"|\"drifting\"|\"off_track\"|\"blocked\", \"goal_restated\": \"the \
current task in one sentence, in the human's own words\", \"gaps\": [\"what the agent is missing \
or has silently dropped from the current task\"], \"correction\": \"one instruction that would \
put the work back on the current task, addressed to the agent, or empty when none is needed\", \
\"confidence\": 0.0}. Rules for the correction: it names what to do next in terms of the current \
task; it never asks for wider permissions, sudo, force pushes, sandbox escapes or the deletion of \
anything; it is empty whenever the verdict is on_track.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    OnTrack,
    Drifting,
    OffTrack,
    Blocked,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::OnTrack => "on_track",
            Verdict::Drifting => "drifting",
            Verdict::OffTrack => "off_track",
            Verdict::Blocked => "blocked",
        }
    }

    pub fn parse(raw: &str) -> Option<Verdict> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "on_track" | "ontrack" | "on-track" => Some(Verdict::OnTrack),
            "drifting" | "drift" => Some(Verdict::Drifting),
            "off_track" | "offtrack" | "off-track" => Some(Verdict::OffTrack),
            "blocked" => Some(Verdict::Blocked),
            _ => None,
        }
    }

    pub fn wants_correction(self) -> bool {
        matches!(self, Verdict::Drifting | Verdict::OffTrack)
    }
}

#[derive(Debug, Clone)]
pub struct Review {
    pub verdict: Verdict,
    pub goal_restated: String,
    pub gaps: Vec<String>,
    pub correction: Option<String>,
    pub confidence: Option<f64>,
}

impl Review {
    pub fn headline(&self) -> String {
        let gaps = if self.gaps.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.gaps.join("; "))
        };
        format!("{}{}", self.verdict.label(), gaps)
    }
}

pub fn parse_review(text: &str) -> Result<Review> {
    let Some(json) = super::decision::extract_json(text) else {
        bail!("reviewer answered with no JSON object");
    };
    let value: Value = serde_json::from_str(&json)?;
    let Some(verdict) = value
        .get("verdict")
        .and_then(Value::as_str)
        .and_then(Verdict::parse)
    else {
        bail!("reviewer answered without a usable verdict");
    };
    let goal_restated = value
        .get("goal_restated")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let gaps = value
        .get("gaps")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|gap| !gap.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let correction = value
        .get("correction")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .filter(|_| verdict.wants_correction());
    let confidence = value.get("confidence").and_then(Value::as_f64);
    Ok(Review {
        verdict,
        goal_restated,
        gaps,
        correction,
        confidence,
    })
}

pub fn reviewer_ids(under_review: Family, rules: &ReviewRules) -> Vec<String> {
    let own = under_review.compact_backend();
    rules
        .reviewers
        .iter()
        .filter(|id| !rules.cross_family_only || id.as_str() != own)
        .filter(|id| Backend::parse(id).is_some())
        .cloned()
        .collect()
}

pub fn reviewer_providers(
    under_review: Family,
    rules: &ReviewRules,
    shared: &Providers,
) -> Providers {
    Providers {
        enabled: reviewer_ids(under_review, rules),
        strategy: Strategy::Priority,
        per_provider: shared.per_provider.clone(),
        budget_usd: shared.budget_usd,
    }
}

const MENU_REVIEWERS: [&str; 5] = [
    "codex-cli",
    "claude-cli",
    "antigravity",
    "cursor-cli",
    "gemini-cli",
];

pub fn menu_reviewers(under_review: Family) -> Vec<&'static str> {
    MENU_REVIEWERS
        .into_iter()
        .filter(|id| *id != under_review.compact_backend())
        .collect()
}

pub fn reviewer_code(id: &str) -> Option<char> {
    match id {
        "codex-cli" => Some('c'),
        "claude-cli" => Some('l'),
        "antigravity" => Some('a'),
        "cursor-cli" => Some('u'),
        "gemini-cli" => Some('g'),
        _ => None,
    }
}

pub fn reviewer_from_code(code: char) -> Option<&'static str> {
    MENU_REVIEWERS
        .into_iter()
        .find(|id| reviewer_code(id) == Some(code))
}

pub fn depth_code(depth: ReviewDepth) -> char {
    match depth {
        ReviewDepth::Shallow => 's',
        ReviewDepth::Normal => 'n',
        ReviewDepth::Deep => 'd',
    }
}

pub fn depth_from_code(code: char) -> Option<ReviewDepth> {
    match code {
        's' => Some(ReviewDepth::Shallow),
        'n' => Some(ReviewDepth::Normal),
        'd' => Some(ReviewDepth::Deep),
        _ => None,
    }
}

pub fn chosen_reviewers(
    under_review: Family,
    rules: &ReviewRules,
    shared: &Providers,
    pick: Option<&str>,
) -> Providers {
    let mut providers = reviewer_providers(under_review, rules, shared);
    if let Some(id) = pick {
        if Backend::parse(id).is_some() && id != under_review.compact_backend() {
            providers.enabled = vec![id.to_string()];
        }
    }
    providers
}

const INSTRUCTION_MIN_WORDS: usize = 4;
const QUESTION_MAX_WORDS: usize = 8;
const RECENT_HUMAN_TURNS: usize = 5;

fn is_instruction(text: &str) -> bool {
    let words = text.split_whitespace().count();
    let question = text.trim_end().ends_with('?');
    words >= INSTRUCTION_MIN_WORDS && !(question && words < QUESTION_MAX_WORDS)
}

fn human_turns(tail: &[(char, String)]) -> Vec<&str> {
    tail.iter()
        .filter(|(role, text)| speaker_of(*role) == "human" && !text.trim().is_empty())
        .map(|(_, text)| text.as_str())
        .collect()
}

pub fn latest_human_ask(tail: &[(char, String)]) -> Option<&str> {
    let humans = human_turns(tail);
    humans
        .iter()
        .rev()
        .find(|text| is_instruction(text))
        .or_else(|| humans.last())
        .copied()
}

fn speaker_of(role: char) -> &'static str {
    match role.to_ascii_uppercase() {
        'U' => "human",
        'A' => "agent",
        'T' => "tool",
        _ => "other",
    }
}

pub fn review_prompt(
    under_review: Family,
    goal: Option<&str>,
    tail: &[(char, String)],
    depth: ReviewDepth,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "Agent under review: {}\nReview depth: {}\n\n",
        under_review.label(),
        depth.label()
    ));
    prompt.push_str("Current task, the most recent instruction the human gave:\n");
    match latest_human_ask(tail) {
        Some(ask) => {
            prompt.push_str(ask.trim());
            prompt.push('\n');
        }
        None => prompt.push_str("(no human instruction inside the readable window)\n"),
    }
    let humans = human_turns(tail);
    if humans.len() > 1 {
        prompt.push_str(
            "\nLatest messages from the human, oldest first (short follow-ups are context, not the task):\n",
        );
        for text in humans
            .iter()
            .skip(humans.len().saturating_sub(RECENT_HUMAN_TURNS))
        {
            prompt.push_str(&format!("- {}\n", text.trim()));
        }
    }
    prompt.push_str("\nBackground only, the goal this session opened with months ago:\n");
    match goal {
        Some(goal) if !goal.trim().is_empty() => {
            prompt.push_str(goal.trim());
            prompt.push('\n');
        }
        _ => prompt.push_str("(not recoverable from the readable part of the transcript)\n"),
    }
    prompt.push_str("\nMost recent work in that session, oldest first:\n");
    for (role, text) in tail {
        prompt.push_str(&format!("[{}] {}\n", speaker_of(*role), text.trim()));
    }
    prompt
}

const CODEX_GOAL_SCAN_LIMIT: u64 = 32 * 1024 * 1024;
const TAIL_WINDOW_BYTES: u64 = 8 * 1024 * 1024;
const TAIL_SCAN_MESSAGES: usize = 400;

pub fn body_with_latest_human(scanned: Vec<(char, String)>, want: usize) -> Vec<(char, String)> {
    let body_from = scanned.len().saturating_sub(want);
    let humans: Vec<usize> = scanned
        .iter()
        .enumerate()
        .filter(|(_, (role, text))| speaker_of(*role) == "human" && !text.trim().is_empty())
        .map(|(at, _)| at)
        .collect();
    let latest = humans.last().copied();
    let instruction = humans
        .iter()
        .rev()
        .copied()
        .find(|at| is_instruction(&scanned[*at].1));
    let mut carried: Vec<usize> = [instruction, latest]
        .into_iter()
        .flatten()
        .filter(|at| *at < body_from)
        .collect();
    carried.sort_unstable();
    carried.dedup();
    let mut body: Vec<(char, String)> = carried.iter().map(|at| scanned[*at].clone()).collect();
    body.extend(scanned.into_iter().skip(body_from));
    body
}

pub fn session_view(
    family: Family,
    transcript: &std::path::Path,
    depth: ReviewDepth,
) -> (Option<String>, Vec<(char, String)>) {
    match family {
        Family::Codex => {
            let weight = std::fs::metadata(transcript)
                .map(|meta| meta.len())
                .unwrap_or(0);
            let goal = (weight <= CODEX_GOAL_SCAN_LIMIT)
                .then(|| {
                    relay_adapters::codex::semantic_steps(transcript)
                        .into_iter()
                        .find(|step| {
                            step.role == relay_compass::StepRole::User
                                && step.user_origin == relay_compass::UserOrigin::Human
                                && relay_compass::is_substantive_goal(&step.text)
                        })
                        .map(|step| step.text)
                })
                .flatten();
            let scanned = relay_adapters::codex::tail_messages(transcript, TAIL_SCAN_MESSAGES);
            (goal, body_with_latest_human(scanned, depth.tail_messages()))
        }
        _ => {
            let scanned = relay_adapters::claude::tail_messages_window(
                transcript,
                TAIL_SCAN_MESSAGES,
                TAIL_WINDOW_BYTES,
            );
            (
                relay_adapters::claude::first_human_goal(transcript, depth.goal_head_bytes()),
                body_with_latest_human(scanned, depth.tail_messages()),
            )
        }
    }
}

pub async fn ask_reviewer(
    pool: &Pool,
    session_id: &str,
    providers: &Providers,
    discovered: &[Discovered],
    prompt: &str,
    now: i64,
) -> Result<(Backend, Review)> {
    if providers.enabled.is_empty() {
        bail!("no reviewer left after excluding the family under review");
    }
    let _ = session_id;
    let (backend, text) = pool
        .improve(providers, discovered, REVIEW_SYSTEM, prompt, now)
        .await?;
    let review = parse_review(&text)?;
    Ok((backend, review))
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ReviewMark {
    #[serde(default)]
    pub last_review_at: i64,
    #[serde(default)]
    pub reviews_done: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReviewStore {
    #[serde(default)]
    entries: std::collections::HashMap<String, ReviewMark>,
}

pub fn review_state_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".vsc-relay")
            .join("robot-review-state.json"),
    )
}

fn review_key(session_id: &str) -> String {
    blake3::hash(session_id.as_bytes()).to_hex().as_str()[..32].to_string()
}

fn load_reviews(path: &std::path::Path) -> ReviewStore {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn review_mark(path: &std::path::Path, session_id: &str) -> ReviewMark {
    load_reviews(path)
        .entries
        .get(&review_key(session_id))
        .copied()
        .unwrap_or_default()
}

pub fn record_review(path: &std::path::Path, session_id: &str, at: i64) -> Result<()> {
    let mut store = load_reviews(path);
    let mark = store.entries.entry(review_key(session_id)).or_default();
    mark.last_review_at = at;
    mark.reviews_done = mark.reviews_done.saturating_add(1);
    crate::fsutil::secure_write(path, &serde_json::to_vec(&store)?)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewPlan {
    Disabled,
    NoReviewer,
    TooSoon,
    BudgetSpent,
    Run,
}

pub fn plan_review(
    rules: &ReviewRules,
    under_review: Family,
    reviews_done: u32,
    last_review_at: Option<i64>,
    now: i64,
) -> ReviewPlan {
    if !rules.enabled {
        return ReviewPlan::Disabled;
    }
    if reviewer_ids(under_review, rules).is_empty() {
        return ReviewPlan::NoReviewer;
    }
    if reviews_done >= rules.max_per_session {
        return ReviewPlan::BudgetSpent;
    }
    if let Some(last) = last_review_at {
        if now - last < rules.every_secs {
            return ReviewPlan::TooSoon;
        }
    }
    ReviewPlan::Run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ReviewRules {
        ReviewRules {
            enabled: true,
            ..ReviewRules::default()
        }
    }

    #[test]
    fn a_reviewer_is_never_the_family_it_reviews() {
        let claude = reviewer_ids(Family::Claude, &rules());
        assert!(
            !claude.contains(&"claude-cli".to_string()),
            "Claude must not grade its own work: {claude:?}"
        );
        assert_eq!(claude.first().map(String::as_str), Some("codex-cli"));

        let codex = reviewer_ids(Family::Codex, &rules());
        assert!(!codex.contains(&"codex-cli".to_string()));
        assert_eq!(codex.first().map(String::as_str), Some("claude-cli"));
    }

    #[test]
    fn same_family_review_is_possible_only_when_asked_for() {
        let mut relaxed = rules();
        relaxed.cross_family_only = false;
        assert!(reviewer_ids(Family::Claude, &relaxed).contains(&"claude-cli".to_string()));
    }

    #[test]
    fn an_unknown_reviewer_name_is_dropped_rather_than_dispatched() {
        let mut typo = rules();
        typo.reviewers = vec!["codex-cli".into(), "not-a-backend".into()];
        assert_eq!(reviewer_ids(Family::Claude, &typo), vec!["codex-cli"]);
    }

    #[test]
    fn cadence_and_budget_both_hold_the_reviewer_back() {
        let rules = rules();
        assert_eq!(
            plan_review(&rules, Family::Claude, 0, None, 1_000),
            ReviewPlan::Run
        );
        assert_eq!(
            plan_review(&rules, Family::Claude, 0, Some(1_000), 1_000 + 899),
            ReviewPlan::TooSoon
        );
        assert_eq!(
            plan_review(&rules, Family::Claude, 0, Some(1_000), 1_000 + 900),
            ReviewPlan::Run
        );
        assert_eq!(
            plan_review(&rules, Family::Claude, 6, None, 1_000),
            ReviewPlan::BudgetSpent
        );
    }

    #[test]
    fn review_is_off_until_it_is_turned_on() {
        assert_eq!(
            plan_review(&ReviewRules::default(), Family::Claude, 0, None, 1_000),
            ReviewPlan::Disabled
        );
    }

    #[test]
    fn a_session_with_no_cross_family_reviewer_left_is_not_reviewed() {
        let mut only_self = rules();
        only_self.reviewers = vec!["claude-cli".into()];
        assert_eq!(
            plan_review(&only_self, Family::Claude, 0, None, 1_000),
            ReviewPlan::NoReviewer
        );
    }

    #[test]
    fn a_verdict_of_on_track_never_carries_a_correction() {
        let review = parse_review(
            r#"{"verdict":"on_track","goal_restated":"ship the parser",
                "gaps":[],"correction":"do something else","confidence":0.9}"#,
        )
        .unwrap();
        assert_eq!(review.verdict, Verdict::OnTrack);
        assert_eq!(
            review.correction, None,
            "an on-track review must not steer the session"
        );
    }

    #[test]
    fn a_drifting_verdict_keeps_its_correction_and_gaps() {
        let review = parse_review(
            r#"prose before {"verdict":"drifting","goal_restated":"ship the parser",
                "gaps":["tests were dropped"],"correction":"restore the failing test first",
                "confidence":0.6} prose after"#,
        )
        .unwrap();
        assert_eq!(review.verdict, Verdict::Drifting);
        assert_eq!(review.gaps, vec!["tests were dropped"]);
        assert_eq!(
            review.correction.as_deref(),
            Some("restore the failing test first")
        );
        assert_eq!(review.confidence, Some(0.6));
    }

    #[test]
    fn an_answer_without_a_verdict_is_refused_rather_than_guessed() {
        assert!(parse_review("the work looks fine to me").is_err());
        assert!(parse_review(r#"{"goal_restated":"ship it"}"#).is_err());
        assert!(parse_review(r#"{"verdict":"maybe","goal_restated":"x"}"#).is_err());
    }

    #[test]
    fn the_prompt_names_the_goal_and_the_family_under_review() {
        let tail = vec![
            ('U', "make the tests pass".to_string()),
            ('A', "I removed the test".to_string()),
        ];
        let prompt = review_prompt(
            Family::Claude,
            Some("make the tests pass"),
            &tail,
            ReviewDepth::Normal,
        );
        assert!(prompt.contains("Claude Code"));
        assert!(prompt.contains("make the tests pass"));
        assert!(prompt.contains("[agent] I removed the test"));
    }

    #[test]
    fn the_task_under_review_is_the_latest_human_turn_not_the_session_opener() {
        let tail = vec![
            ('U', "build the relay".to_string()),
            ('A', "done".to_string()),
            ('U', "now add cross review".to_string()),
            ('A', "reading the pool".to_string()),
        ];
        assert_eq!(latest_human_ask(&tail), Some("now add cross review"));
        let prompt = review_prompt(
            Family::Claude,
            Some("a goal stated back in July"),
            &tail,
            ReviewDepth::Normal,
        );
        let task_at = prompt.find("Current task").expect("current task section");
        let background_at = prompt.find("Background only").expect("background section");
        assert!(
            task_at < background_at,
            "the current task is stated before the session opener, never instead of it"
        );
        assert!(prompt[task_at..background_at].contains("now add cross review"));
        assert!(prompt[background_at..].contains("a goal stated back in July"));
    }

    #[test]
    fn transcript_roles_are_read_in_the_case_the_adapters_actually_emit() {
        let tail = vec![
            ('U', "the real ask".to_string()),
            ('T', "Bash".to_string()),
            ('A', "working".to_string()),
        ];
        assert_eq!(latest_human_ask(&tail), Some("the real ask"));
        let prompt = review_prompt(Family::Claude, None, &tail, ReviewDepth::Normal);
        assert!(prompt.contains("[human] the real ask"));
        assert!(prompt.contains("[tool] Bash"));
        assert!(prompt.contains("[agent] working"));
        assert!(
            !prompt.contains("[other]"),
            "every role the adapters emit must be named, not filed as other"
        );
    }

    #[test]
    fn the_latest_human_turn_is_carried_in_even_when_tool_calls_crowd_the_tail() {
        let mut scanned = vec![('U', "the ask that matters".to_string())];
        for n in 0..30 {
            scanned.push(('T', format!("Bash {n}")));
        }
        let body = body_with_latest_human(scanned, 5);
        assert_eq!(body.len(), 6, "the human turn is added, not swapped in");
        assert_eq!(latest_human_ask(&body), Some("the ask that matters"));
        assert_eq!(body.last().map(|(_, text)| text.as_str()), Some("Bash 29"));
    }

    #[test]
    fn a_human_turn_already_inside_the_body_is_not_duplicated() {
        let scanned = vec![
            ('T', "Bash 1".to_string()),
            ('U', "the ask".to_string()),
            ('A', "working".to_string()),
        ];
        let body = body_with_latest_human(scanned, 3);
        assert_eq!(body.len(), 3);
        assert_eq!(
            body.iter().filter(|(role, _)| *role == 'U').count(),
            1,
            "the human turn appears once"
        );
    }

    #[test]
    fn a_short_follow_up_question_does_not_replace_the_instruction_it_follows() {
        let tail = vec![
            (
                'U',
                "recount the monthly totals per plant and write them to the report".to_string(),
            ),
            ('A', "counting".to_string()),
            ('U', "когда посчитается?".to_string()),
        ];
        assert_eq!(
            latest_human_ask(&tail),
            Some("recount the monthly totals per plant and write them to the report")
        );
        let prompt = review_prompt(Family::Claude, None, &tail, ReviewDepth::Normal);
        let task_at = prompt.find("Current task").expect("current task section");
        let follow_ups_at = prompt
            .find("Latest messages from the human")
            .expect("follow-ups");
        assert!(prompt[task_at..follow_ups_at].contains("recount the monthly totals"));
        assert!(prompt[follow_ups_at..].contains("когда посчитается?"));
    }

    #[test]
    fn a_lone_short_message_is_still_used_when_nothing_longer_exists() {
        let tail = vec![('U', "fix it".to_string())];
        assert_eq!(latest_human_ask(&tail), Some("fix it"));
    }

    #[test]
    fn an_old_instruction_and_a_new_follow_up_both_reach_the_reviewer() {
        let mut scanned = vec![('U', "recount the monthly totals per plant".to_string())];
        scanned.extend((0..20).map(|n| ('T', format!("Bash {n}"))));
        scanned.push(('U', "когда посчитается?".to_string()));
        scanned.extend((20..40).map(|n| ('T', format!("Bash {n}"))));
        let body = body_with_latest_human(scanned, 5);
        assert_eq!(body.len(), 7);
        assert_eq!(
            latest_human_ask(&body),
            Some("recount the monthly totals per plant")
        );
        assert!(body.iter().any(|(_, text)| text == "когда посчитается?"));
    }

    #[test]
    fn an_explicit_reviewer_pick_is_honoured_but_never_the_family_under_review() {
        let shared = Providers::default();
        let picked = chosen_reviewers(Family::Claude, &rules(), &shared, Some("antigravity"));
        assert_eq!(picked.enabled, vec!["antigravity"]);
        let own = chosen_reviewers(Family::Claude, &rules(), &shared, Some("claude-cli"));
        assert!(!own.enabled.contains(&"claude-cli".to_string()));
        let default = chosen_reviewers(Family::Codex, &rules(), &shared, None);
        assert!(!default.enabled.contains(&"codex-cli".to_string()));
    }

    #[test]
    fn button_codes_round_trip_for_every_reviewer_and_depth() {
        for id in MENU_REVIEWERS {
            let code = reviewer_code(id).expect("every menu reviewer has a code");
            assert_eq!(reviewer_from_code(code), Some(id));
        }
        assert_eq!(reviewer_from_code('p'), None, "p means the default order");
        for depth in [ReviewDepth::Shallow, ReviewDepth::Normal, ReviewDepth::Deep] {
            assert_eq!(depth_from_code(depth_code(depth)), Some(depth));
        }
        assert!(!menu_reviewers(Family::Codex).contains(&"codex-cli"));
        assert!(!menu_reviewers(Family::Claude).contains(&"claude-cli"));
    }

    #[test]
    fn a_window_with_no_human_turn_says_so_instead_of_inventing_a_task() {
        let tail = vec![('A', "still working".to_string())];
        assert_eq!(latest_human_ask(&tail), None);
        let prompt = review_prompt(Family::Codex, None, &tail, ReviewDepth::Shallow);
        assert!(prompt.contains("no human instruction inside the readable window"));
    }

    #[test]
    fn a_missing_goal_is_declared_rather_than_invented() {
        let prompt = review_prompt(Family::Codex, None, &[], ReviewDepth::Shallow);
        assert!(prompt.contains("not recoverable"));
    }

    #[test]
    fn cadence_survives_a_daemon_restart_and_stores_no_session_id() {
        let path = std::env::temp_dir().join(format!(
            "vsc-relay-review-state-{}-{}.json",
            std::process::id(),
            crate::automation::now_secs()
        ));
        let _ = std::fs::remove_file(&path);
        let session = "sensitive-native-session-id";

        assert_eq!(review_mark(&path, session).reviews_done, 0);
        record_review(&path, session, 1_000).unwrap();
        record_review(&path, session, 2_000).unwrap();

        let mark = review_mark(&path, session);
        assert_eq!(mark.reviews_done, 2);
        assert_eq!(mark.last_review_at, 2_000);
        assert_eq!(
            plan_review(
                &rules(),
                Family::Claude,
                mark.reviews_done,
                Some(mark.last_review_at),
                2_100
            ),
            ReviewPlan::TooSoon,
            "a restart must not reset the cadence and refire on every session"
        );

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains(session));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reviewers_inherit_models_and_budget_but_not_the_enabled_list() {
        let mut shared = Providers {
            enabled: vec!["ollama".to_string()],
            budget_usd: Some(3.0),
            ..Providers::default()
        };
        shared.set_model("codex-cli", "gpt-5");
        let providers = reviewer_providers(Family::Claude, &rules(), &shared);
        assert_eq!(providers.enabled, vec!["codex-cli", "antigravity"]);
        assert_eq!(providers.budget_usd, Some(3.0));
        assert_eq!(
            providers
                .per_provider
                .get("codex-cli")
                .and_then(|opts| opts.model.clone()),
            Some("gpt-5".to_string())
        );
    }
}

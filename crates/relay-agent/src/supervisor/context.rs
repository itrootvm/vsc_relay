use relay_core::state::{truncate, AskUserQuestion};

pub struct SessionSnapshot<'a> {
    pub alias: &'a str,
    pub agent: &'a str,
    pub state_label: &'a str,
    pub last_message: &'a str,
    pub tail: &'a [(char, String)],
    pub question: Option<&'a AskUserQuestion>,
    pub goal: Option<&'a str>,
}

fn render_question(q: &AskUserQuestion) -> String {
    let mut out =
        String::from("QUESTION (answer with option_index, counting from 0 across all):\n");
    let mut idx = 0usize;
    for sub in &q.questions {
        out.push_str(&format!("- {}\n", truncate(&sub.question, 300)));
        for opt in &sub.options {
            out.push_str(&format!("    [{idx}] {}\n", truncate(&opt.label, 120)));
            idx += 1;
        }
    }
    out
}

pub fn build_context(snap: &SessionSnapshot, budget: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Session: {} ({})\nState: {}\n",
        snap.alias, snap.agent, snap.state_label
    ));
    if let Some(goal) = snap.goal.filter(|g| !g.trim().is_empty()) {
        out.push_str(&format!("Goal: {}\n", truncate(goal, 400)));
    }
    if let Some(q) = snap.question {
        out.push('\n');
        out.push_str(&render_question(q));
    }

    let head_len = out.len();
    let remaining = budget.saturating_sub(head_len);
    let last = truncate(snap.last_message.trim(), remaining.min(1200));
    if !last.is_empty() {
        out.push_str(&format!("\nLast message:\n{last}\n"));
    }

    if !snap.tail.is_empty() {
        let mut convo = String::from("\nRecent conversation:\n");
        for (who, msg) in snap.tail {
            let role = if *who == 'U' { "USER" } else { "AGENT" };
            convo.push_str(&format!("{role}: {}\n", truncate(msg.trim(), 400)));
        }
        let room = budget.saturating_sub(out.len());
        out.push_str(&truncate(&convo, room));
    }

    truncate(&out, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::state::{QOption, Question};

    fn q() -> AskUserQuestion {
        AskUserQuestion {
            questions: vec![Question {
                header: "h".into(),
                question: "Ship it?".into(),
                multi_select: false,
                options: vec![
                    QOption {
                        label: "Yes".into(),
                        description: String::new(),
                    },
                    QOption {
                        label: "No".into(),
                        description: String::new(),
                    },
                ],
            }],
            tool_use_id: None,
        }
    }

    #[test]
    fn includes_header_state_and_question_options() {
        let snap = SessionSnapshot {
            alias: "proj",
            agent: "claude",
            state_label: "pending_question",
            last_message: "waiting",
            tail: &[],
            question: Some(&q()),
            goal: Some("land the PR"),
        };
        let c = build_context(&snap, 4000);
        assert!(c.contains("Session: proj (claude)"));
        assert!(c.contains("State: pending_question"));
        assert!(c.contains("Goal: land the PR"));
        assert!(c.contains("[0] Yes"));
        assert!(c.contains("[1] No"));
    }

    #[test]
    fn respects_budget() {
        let big = "x ".repeat(5000);
        let tail = vec![('U', big.clone()), ('A', big.clone())];
        let snap = SessionSnapshot {
            alias: "p",
            agent: "claude",
            state_label: "idle",
            last_message: &big,
            tail: &tail,
            question: None,
            goal: None,
        };
        let c = build_context(&snap, 1500);
        assert!(
            c.chars().count() <= 1501,
            "context {} chars exceeds budget",
            c.chars().count()
        );
        assert!(c.contains("Session: p (claude)"));
    }

    #[test]
    fn no_question_no_goal_is_clean() {
        let snap = SessionSnapshot {
            alias: "p",
            agent: "codex",
            state_label: "idle",
            last_message: "done",
            tail: &[],
            question: None,
            goal: None,
        };
        let c = build_context(&snap, 2000);
        assert!(!c.contains("QUESTION"));
        assert!(!c.contains("Goal:"));
        assert!(c.contains("Last message:\ndone"));
    }
}

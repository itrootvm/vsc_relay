use relay_compass::{MutationKind, ToolEffect};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectSource {
    StructuredPayload,
    CapabilityAnnotation,
    CompatibilityFallback,
    ShellSegmented,
    UnknownShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectAssessment {
    pub effect: ToolEffect,
    pub target: Option<String>,
    pub source: EffectSource,
}

fn string_field<'a>(input: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn target(input: &Value) -> Option<String> {
    string_field(
        input,
        &[
            "file_path",
            "path",
            "target",
            "uri",
            "resource",
            "notebook_path",
            "destination",
        ],
    )
    .map(str::to_string)
}

fn resolved_target(cwd: &Path, target: &str) -> PathBuf {
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn annotated_effect(input: &Value) -> Option<ToolEffect> {
    let annotations = input
        .get("annotations")
        .or_else(|| input.get("capabilities"))
        .unwrap_or(input);
    if annotations
        .get("readOnlyHint")
        .or_else(|| annotations.get("read_only"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Some(ToolEffect::ReadOnly);
    }
    if annotations
        .get("destructiveHint")
        .or_else(|| annotations.get("destructive"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Some(ToolEffect::Mutation(MutationKind::Delete));
    }
    None
}

fn patch_effect(patch: &str) -> Option<(ToolEffect, Option<String>)> {
    let mut adds = Vec::new();
    let mut deletes = Vec::new();
    let mut updates = Vec::new();
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            adds.push(path.trim().to_string());
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            deletes.push(path.trim().to_string());
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            updates.push(path.trim().to_string());
        }
    }
    let total = adds.len() + deletes.len() + updates.len();
    if total == 0 {
        return None;
    }
    if total > 1 {
        let target = adds
            .first()
            .or_else(|| deletes.first())
            .or_else(|| updates.first())
            .cloned();
        return Some((ToolEffect::Mutation(MutationKind::Migrate), target));
    }
    if let Some(path) = adds.pop() {
        return Some((ToolEffect::Mutation(MutationKind::Create), Some(path)));
    }
    if let Some(path) = deletes.pop() {
        return Some((ToolEffect::Mutation(MutationKind::Delete), Some(path)));
    }
    updates
        .pop()
        .map(|path| (ToolEffect::Mutation(MutationKind::Modify), Some(path)))
}

fn structured_effect(input: &Value, cwd: &Path) -> Option<(ToolEffect, Option<String>)> {
    if input
        .get("plan")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            !items.is_empty()
                && items.iter().all(|item| {
                    item.as_object().is_some_and(|fields| {
                        fields.get("step").and_then(Value::as_str).is_some()
                            && fields.get("status").and_then(Value::as_str).is_some()
                            && fields
                                .keys()
                                .all(|key| matches!(key.as_str(), "step" | "status"))
                    })
                })
                && input.as_object().is_some_and(|fields| {
                    fields
                        .keys()
                        .all(|key| matches!(key.as_str(), "plan" | "explanation"))
                })
        })
    {
        return Some((ToolEffect::ReadOnly, None));
    }
    if let Some(method) = input.get("method").and_then(Value::as_str) {
        if !matches!(
            method.to_ascii_uppercase().as_str(),
            "GET" | "HEAD" | "OPTIONS"
        ) {
            return Some((
                ToolEffect::ExternalSideEffect,
                string_field(input, &["url", "uri"]).map(str::to_string),
            ));
        }
    }
    if (input.get("recipient").is_some()
        || input.get("channel").is_some()
        || input.get("chat_id").is_some())
        && (input.get("message").is_some()
            || input.get("body").is_some()
            || input.get("content").is_some())
    {
        return Some((ToolEffect::ExternalSideEffect, target(input)));
    }
    if input.get("url").is_some() && input.get("method").is_none() {
        return Some((
            ToolEffect::ReadOnly,
            input.get("url").and_then(Value::as_str).map(str::to_string),
        ));
    }
    if let Some(patch) = string_field(input, &["patch", "diff", "input"]) {
        if let Some(effect) = patch_effect(patch) {
            return Some(effect);
        }
    }

    if let Some(command) = string_field(input, &["command", "cmd"]) {
        let trimmed = command.trim();
        if trimmed.starts_with("*** Begin Patch") && trimmed.ends_with("*** End Patch") {
            if let Some(effect) = patch_effect(trimmed) {
                return Some(effect);
            }
        }
    }

    let target = target(input);
    if input.get("old_string").is_some()
        || input.get("new_string").is_some()
        || input.get("edits").is_some()
        || input.get("operations").is_some()
    {
        return Some((ToolEffect::Mutation(MutationKind::Modify), target));
    }
    if let Some(target) = target.as_deref() {
        if input.get("content").is_some()
            || input.get("contents").is_some()
            || input.get("data").is_some()
        {
            let kind = if resolved_target(cwd, target).exists() {
                MutationKind::Replace
            } else {
                MutationKind::Create
            };
            return Some((ToolEffect::Mutation(kind), Some(target.to_string())));
        }
    }
    if input.get("query").is_some()
        || input.get("pattern").is_some()
        || input.get("cursor").is_some()
        || input.get("page").is_some()
        || input.get("line_start").is_some()
        || input.get("offset").is_some()
    {
        return Some((ToolEffect::ReadOnly, target));
    }
    if target.is_some()
        && input.as_object().is_some_and(|map| {
            map.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "file_path"
                        | "path"
                        | "target"
                        | "uri"
                        | "resource"
                        | "notebook_path"
                        | "offset"
                        | "limit"
                        | "line_start"
                        | "line_end"
                )
            })
        })
    {
        return Some((ToolEffect::ReadOnly, target));
    }
    None
}

fn clean_shell_target(token: &str) -> Option<String> {
    let target = token
        .trim()
        .trim_matches(|character| matches!(character, '\'' | '"'))
        .trim_end_matches([';', '&']);
    (!target.is_empty()
        && !target.contains(['\n', '\r', '|', '<', '>', '$', '`'])
        && !matches!(target, "/dev/null" | "&1" | "&2"))
    .then(|| target.to_string())
}

fn path_mutation(cwd: &Path, target: String, existing: MutationKind) -> EffectAssessment {
    let kind = if resolved_target(cwd, &target).exists() {
        existing
    } else {
        MutationKind::Create
    };
    EffectAssessment {
        effect: ToolEffect::Mutation(kind),
        target: Some(target),
        source: EffectSource::CompatibilityFallback,
    }
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn shell_tokens(command: &str) -> Vec<&str> {
    let raw = command.split_whitespace().collect::<Vec<_>>();
    let start = raw
        .iter()
        .position(|token| !is_env_assignment(token))
        .unwrap_or(raw.len());
    raw.into_iter().skip(start).collect()
}

fn unknown_shape() -> EffectAssessment {
    EffectAssessment {
        effect: ToolEffect::Unknown,
        target: None,
        source: EffectSource::UnknownShape,
    }
}

fn shell_segments(command: &str) -> Option<Vec<&str>> {
    if command.contains("<<") {
        return None;
    }
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<u8> = None;
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) => {
                if byte == b'\\' && open == b'"' {
                    at += 2;
                    continue;
                }
                if byte == open {
                    quote = None;
                }
                at += 1;
            }
            None => match byte {
                b'\'' | b'"' => {
                    quote = Some(byte);
                    at += 1;
                }
                b'\\' => at += 2,
                b'&' if bytes.get(at + 1) == Some(&b'&') => {
                    segments.push(&command[start..at]);
                    at += 2;
                    start = at;
                }
                b'|' if bytes.get(at + 1) == Some(&b'|') => {
                    segments.push(&command[start..at]);
                    at += 2;
                    start = at;
                }
                b'|' | b';' | b'\n' => {
                    segments.push(&command[start..at]);
                    at += 1;
                    start = at;
                }
                _ => at += 1,
            },
        }
    }
    segments.push(command.get(start..).unwrap_or_default());
    Some(
        segments
            .into_iter()
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect(),
    )
}

fn is_navigation(segment: &str) -> bool {
    matches!(
        shell_tokens(segment).first().copied(),
        Some("cd" | "pushd" | "popd")
    )
}

fn shell_fallback(command: &str, cwd: &Path) -> EffectAssessment {
    let Some(segments) = shell_segments(command) else {
        return unknown_shape();
    };
    let commands: Vec<&str> = segments
        .iter()
        .copied()
        .filter(|segment| !is_navigation(segment))
        .collect();
    if commands.is_empty() {
        return EffectAssessment {
            effect: ToolEffect::ReadOnly,
            target: None,
            source: EffectSource::ShellSegmented,
        };
    }
    let segmented = commands.len() > 1 || commands.len() != segments.len();
    if !segmented {
        return classify_segment(commands[0], cwd);
    }
    let mut unknown = false;
    for segment in &commands {
        let assessment = classify_segment(segment, cwd);
        match assessment.effect {
            ToolEffect::Mutation(_) | ToolEffect::ExternalSideEffect => {
                return EffectAssessment {
                    source: EffectSource::ShellSegmented,
                    ..assessment
                }
            }
            ToolEffect::Unknown => unknown = true,
            ToolEffect::ReadOnly => {}
        }
    }
    if unknown {
        return unknown_shape();
    }
    EffectAssessment {
        effect: ToolEffect::ReadOnly,
        target: None,
        source: EffectSource::ShellSegmented,
    }
}

fn classify_segment(command: &str, cwd: &Path) -> EffectAssessment {
    let tokens = shell_tokens(command);
    let normalized = tokens.join(" ").to_ascii_lowercase();
    if normalized.is_empty() {
        return unknown_shape();
    }
    if let Some((_, raw_target)) = command.rsplit_once(">>") {
        if let Some(target) = raw_target
            .split_whitespace()
            .next()
            .and_then(clean_shell_target)
        {
            return path_mutation(cwd, target, MutationKind::Modify);
        }
    } else if let Some((_, raw_target)) = command.rsplit_once('>') {
        if let Some(target) = raw_target
            .split_whitespace()
            .next()
            .and_then(clean_shell_target)
        {
            return path_mutation(cwd, target, MutationKind::Replace);
        }
    }
    let executable = tokens.first().map(|token| {
        token
            .rsplit('/')
            .next()
            .unwrap_or(token)
            .to_ascii_lowercase()
    });
    let last_target = tokens.last().and_then(|token| clean_shell_target(token));
    match executable.as_deref() {
        Some("cp") => {
            if let Some(target) = last_target {
                return path_mutation(cwd, target, MutationKind::Replace);
            }
        }
        Some("touch" | "mkdir") => {
            if let Some(target) = last_target {
                return path_mutation(cwd, target, MutationKind::Modify);
            }
        }
        Some("mv") => {
            if let Some(target) = last_target {
                return EffectAssessment {
                    effect: ToolEffect::Mutation(MutationKind::Migrate),
                    target: Some(target),
                    source: EffectSource::CompatibilityFallback,
                };
            }
        }
        Some("rm" | "rmdir") => {
            return EffectAssessment {
                effect: ToolEffect::Mutation(MutationKind::Delete),
                target: last_target,
                source: EffectSource::CompatibilityFallback,
            };
        }
        Some("git") if tokens.get(1).is_some_and(|token| *token == "mv") => {
            if let Some(target) = last_target {
                return EffectAssessment {
                    effect: ToolEffect::Mutation(MutationKind::Migrate),
                    target: Some(target),
                    source: EffectSource::CompatibilityFallback,
                };
            }
        }
        Some("git")
            if tokens
                .get(1)
                .is_some_and(|token| matches!(*token, "push" | "commit")) =>
        {
            return EffectAssessment {
                effect: ToolEffect::ExternalSideEffect,
                target: None,
                source: EffectSource::CompatibilityFallback,
            };
        }
        Some("ssh" | "scp" | "rsync" | "curl" | "wget") => {
            return EffectAssessment {
                effect: ToolEffect::ExternalSideEffect,
                target: None,
                source: EffectSource::CompatibilityFallback,
            };
        }
        Some("sed")
            if tokens
                .iter()
                .any(|token| token.starts_with("-i") || *token == "--in-place") =>
        {
            return EffectAssessment {
                effect: ToolEffect::Mutation(MutationKind::Modify),
                target: last_target,
                source: EffectSource::CompatibilityFallback,
            };
        }
        _ => {}
    }
    let safe_prefixes = [
        "rg ",
        "rg --",
        "grep ",
        "cat ",
        "head ",
        "tail ",
        "ls ",
        "find ",
        "stat ",
        "wc ",
        "awk ",
        "df ",
        "du ",
        "ps ",
        "pgrep ",
        "shasum ",
        "file ",
        "which ",
        "sort",
        "uniq",
        "cut ",
        "tr ",
        "nl ",
        "seq ",
        "diff ",
        "cmp ",
        "jq ",
        "sed ",
        "printf ",
        "echo ",
        "pwd",
        "date",
        "uname",
        "basename ",
        "dirname ",
        "sleep ",
        "git status",
        "git diff",
        "git log",
        "git show",
        "cargo check",
        "cargo test",
        "cargo clippy",
    ];
    if safe_prefixes
        .iter()
        .any(|prefix| normalized == prefix.trim() || normalized.starts_with(prefix))
    {
        EffectAssessment {
            effect: ToolEffect::ReadOnly,
            target: None,
            source: EffectSource::CompatibilityFallback,
        }
    } else {
        unknown_shape()
    }
}

pub fn is_relay_control_plane(payload: &Value) -> bool {
    let Some(command) = payload
        .get("tool_input")
        .and_then(|input| string_field(input, &["command", "cmd"]))
    else {
        return false;
    };
    let tokens = shell_tokens(command);
    let Some(executable) = tokens.first().and_then(|token| token.rsplit('/').next()) else {
        return false;
    };
    if executable != "vsc-relay-agent" || tokens.get(1) != Some(&"automation") {
        return false;
    }
    match tokens.get(2).copied() {
        Some(
            "set-default" | "set-workspace" | "set-session" | "clear-workspace" | "clear-session"
            | "provider" | "strategy" | "provider-model" | "provider-key" | "get" | "list"
            | "discover" | "models" | "health",
        ) => true,
        Some("smart") => matches!(
            tokens.get(3).copied(),
            Some(
                "status"
                    | "check"
                    | "on"
                    | "off"
                    | "steer"
                    | "gate"
                    | "feedback"
                    | "provider"
                    | "model"
                    | "endpoint"
                    | "local-dir"
                    | "trust"
                    | "key"
            )
        ),
        _ => false,
    }
}

pub fn classify(payload: &Value) -> EffectAssessment {
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    if let Some((effect, target)) = structured_effect(input, cwd) {
        return EffectAssessment {
            effect,
            target,
            source: EffectSource::StructuredPayload,
        };
    }
    if let Some(effect) = annotated_effect(input) {
        return EffectAssessment {
            effect,
            target: target(input),
            source: EffectSource::CapabilityAnnotation,
        };
    }
    if let Some(command) = string_field(input, &["command", "cmd"]) {
        return shell_fallback(command, cwd);
    }
    EffectAssessment {
        effect: ToolEffect::Unknown,
        target: target(input),
        source: EffectSource::UnknownShape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renamed_tools_with_same_shape_have_same_effect() {
        let a = json!({"tool_name":"Write", "cwd":"/tmp", "tool_input":{"file_path":"x", "content":"a"}});
        let b = json!({"tool_name":"future_tool_92", "cwd":"/tmp", "tool_input":{"file_path":"x", "content":"a"}});
        assert_eq!(classify(&a).effect, classify(&b).effect);
        assert_eq!(classify(&a).target, classify(&b).target);
    }

    #[test]
    fn ordinary_edit_is_modify_not_novelty() {
        let payload = json!({"tool_name":"anything", "tool_input":{
            "file_path":"src/lib.rs", "old_string":"old", "new_string":"new"
        }});
        assert_eq!(
            classify(&payload).effect,
            ToolEffect::Mutation(MutationKind::Modify)
        );
    }

    #[test]
    fn unknown_shape_does_not_invent_capability_from_name() {
        let payload = json!({"tool_name":"Write", "tool_input":{"opaque":7}});
        assert_eq!(classify(&payload).effect, ToolEffect::Unknown);
    }

    #[test]
    fn apply_patch_has_structural_mutation_kind() {
        let create =
            json!({"tool_input":{"patch":"*** Begin Patch\n*** Add File: x\n+a\n*** End Patch"}});
        assert_eq!(
            classify(&create).effect,
            ToolEffect::Mutation(MutationKind::Create)
        );
        assert_eq!(classify(&create).target.as_deref(), Some("x"));
    }

    #[test]
    fn multi_file_patch_binds_a_target_so_it_cannot_bypass_the_gate() {
        let patch =
            "*** Begin Patch\n*** Add File: a.rs\n+a\n*** Add File: b.rs\n+b\n*** End Patch";
        let assessment = classify(&json!({"tool_input":{"patch":patch}}));
        assert_eq!(
            assessment.effect,
            ToolEffect::Mutation(MutationKind::Migrate)
        );
        assert_eq!(assessment.target.as_deref(), Some("a.rs"));
    }

    #[test]
    fn native_codex_patch_command_is_classified_by_envelope_not_tool_name() {
        let patch = "*** Begin Patch\n*** Add File: gate-native.txt\n+ok\n*** End Patch";
        let canonical = json!({"tool_name":"apply_patch", "tool_input":{"command":patch}});
        let renamed = json!({"tool_name":"future_patch_tool", "tool_input":{"command":patch}});
        for payload in [canonical, renamed] {
            let assessment = classify(&payload);
            assert_eq!(
                assessment.effect,
                ToolEffect::Mutation(MutationKind::Create)
            );
            assert_eq!(assessment.target.as_deref(), Some("gate-native.txt"));
            assert_eq!(assessment.source, EffectSource::StructuredPayload);
        }
    }

    #[test]
    fn shell_redirection_recovers_target_and_novelty_without_tool_name() {
        let payload = json!({
            "tool_name":"opaque-shell-wrapper",
            "cwd":"/tmp",
            "tool_input":{"command":"printf data > gate-new-file-unique.txt"}
        });
        let assessment = classify(&payload);
        assert_eq!(
            assessment.effect,
            ToolEffect::Mutation(MutationKind::Create)
        );
        assert_eq!(
            assessment.target.as_deref(),
            Some("gate-new-file-unique.txt")
        );
    }

    #[test]
    fn ambiguous_shell_command_remains_unknown() {
        let payload = json!({"tool_input":{"command":"future-command --opaque"}});
        assert_eq!(classify(&payload).effect, ToolEffect::Unknown);
    }

    #[test]
    fn controller_plan_shape_is_non_artifact_effect_for_any_tool_name() {
        let input = json!({
            "plan": [
                {"step":"inspect", "status":"in_progress"},
                {"step":"verify", "status":"pending"}
            ],
            "explanation":"runtime-only state"
        });
        for name in ["update_plan", "future_controller_17"] {
            let payload = json!({"tool_name":name, "tool_input":input});
            let assessment = classify(&payload);
            assert_eq!(assessment.effect, ToolEffect::ReadOnly);
            assert!(assessment.target.is_none());
            assert_eq!(assessment.source, EffectSource::StructuredPayload);
        }
    }

    #[test]
    fn leading_environment_assignment_preserves_read_only_effect() {
        let payload = json!({"tool_input":{"command":"RUST_LOG=info cargo test -p relay-agent"}});
        assert_eq!(classify(&payload).effect, ToolEffect::ReadOnly);
    }

    #[test]
    fn only_deterministic_relay_configuration_is_control_plane() {
        let smart = json!({"tool_input":{"command":"RUST_LOG=info /tmp/vsc-relay-agent automation smart steer off"}});
        let training = json!({"tool_input":{"command":"/tmp/vsc-relay-agent automation smart train-local data out"}});
        let other = json!({"tool_input":{"command":"other-agent automation smart off"}});
        assert!(is_relay_control_plane(&smart));
        assert!(!is_relay_control_plane(&training));
        assert!(!is_relay_control_plane(&other));
    }

    #[test]
    fn a_compound_command_is_read_through_its_navigation_prefix() {
        let cwd = std::env::temp_dir();
        let cases: Vec<(&str, ToolEffect, EffectSource)> = vec![
            (
                "cd /repo && grep -rn needle src",
                ToolEffect::ReadOnly,
                EffectSource::ShellSegmented,
            ),
            (
                "cd /repo && cargo test --workspace",
                ToolEffect::ReadOnly,
                EffectSource::ShellSegmented,
            ),
            (
                "cd /repo && grep foo bar | wc -l",
                ToolEffect::ReadOnly,
                EffectSource::ShellSegmented,
            ),
            (
                "cd /repo; ls -la",
                ToolEffect::ReadOnly,
                EffectSource::ShellSegmented,
            ),
            (
                "cd /repo && ssh host uptime",
                ToolEffect::ExternalSideEffect,
                EffectSource::ShellSegmented,
            ),
            (
                "cd /repo && future-command --opaque",
                ToolEffect::Unknown,
                EffectSource::UnknownShape,
            ),
            (
                "grep -rn needle src",
                ToolEffect::ReadOnly,
                EffectSource::CompatibilityFallback,
            ),
            (
                "future-command --opaque",
                ToolEffect::Unknown,
                EffectSource::UnknownShape,
            ),
            (
                "cd /repo",
                ToolEffect::ReadOnly,
                EffectSource::ShellSegmented,
            ),
        ];
        for (command, effect, source) in cases {
            let got = shell_fallback(command, &cwd);
            assert_eq!(got.effect, effect, "effect of {command:?}");
            assert_eq!(got.source, source, "source of {command:?}");
        }
    }

    #[test]
    fn a_mutation_anywhere_in_the_chain_is_the_effect_of_the_chain() {
        let cwd = std::env::temp_dir();
        let got = shell_fallback("cd /repo && grep x y && rm -rf build", &cwd);
        assert!(
            matches!(got.effect, ToolEffect::Mutation(MutationKind::Delete)),
            "a read-only step never softens a destructive one: {:?}",
            got.effect
        );
    }

    #[test]
    fn a_heredoc_body_is_never_read_as_commands() {
        let cwd = std::env::temp_dir();
        let got = shell_fallback("python3 - <<'PY'\nrm -rf /\nPY", &cwd);
        assert_eq!(
            got.effect,
            ToolEffect::Unknown,
            "the body of a heredoc is data; classifying it would invent an effect"
        );
    }

    #[test]
    fn a_stderr_redirect_is_not_mistaken_for_a_file_target() {
        let cwd = std::env::temp_dir();
        let got = shell_fallback("cd /repo && grep -rn needle src 2>&1", &cwd);
        assert_eq!(
            got.effect,
            ToolEffect::ReadOnly,
            "2>&1 redirects a stream, not a file: {got:?}"
        );
        assert_eq!(got.target, None);
    }

    #[test]
    fn a_real_redirect_still_binds_its_file() {
        let cwd = std::env::temp_dir();
        let got = shell_fallback("cd /repo && printf data > out.txt", &cwd);
        assert!(
            matches!(got.effect, ToolEffect::Mutation(_)),
            "a write through a redirect survives the navigation prefix: {got:?}"
        );
        assert_eq!(got.target.as_deref(), Some("out.txt"));
    }

    #[test]
    fn every_in_place_spelling_of_sed_is_a_mutation() {
        let cwd = std::env::temp_dir();
        for command in [
            "sed -i 's/a/b/' notes.txt",
            "sed -i.bak 's/a/b/' notes.txt",
            "sed --in-place 's/a/b/' notes.txt",
            "cd /repo && sed -i.bak 's/a/b/' notes.txt",
        ] {
            let got = shell_fallback(command, &cwd);
            assert!(
                matches!(got.effect, ToolEffect::Mutation(_)),
                "{command:?} rewrites the file: {got:?}"
            );
        }
        assert_eq!(
            shell_fallback("sed -n '1,5p' notes.txt", &cwd).effect,
            ToolEffect::ReadOnly,
            "printing a range writes nothing"
        );
    }

    #[test]
    fn quotes_hide_separators_from_the_splitter() {
        assert_eq!(
            shell_segments("grep 'a && b' file").expect("segments"),
            vec!["grep 'a && b' file"],
            "a separator inside quotes is text, not a new command"
        );
        assert_eq!(
            shell_segments("cd x && ls").expect("segments"),
            vec!["cd x", "ls"]
        );
    }
}

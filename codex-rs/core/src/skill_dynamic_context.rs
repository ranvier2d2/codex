use std::cmp::min;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_core_skills::SkillDynamicContext;
use codex_core_skills::dynamic_context::InlineCommandPlaceholder;
use codex_core_skills::dynamic_context::collect_inline_command_placeholders;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::protocol::SkillScope;
use codex_tools::ToolName;
use serde_json::Value;

use crate::exec_env::create_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::hook_runtime::PreToolUseHookResult;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::run_post_tool_use_hooks;
use crate::hook_runtime::run_pre_tool_use_hooks;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::skills::SkillLoadOutcome;
use crate::skills::SkillMetadata;
use crate::skills::injection::SkillInjection;
use crate::tools::format_exec_output_str;
use crate::tools::hook_names::HookToolName;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;

const DEFAULT_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 20_000;
const DEFAULT_MAX_TOTAL_OUTPUT_CHARS: usize = 50_000;
const DEFAULT_MAX_PLACEHOLDERS: usize = 16;
const MAX_TOTAL_RUNTIME_SECONDS: u64 = 120;

static PROCESS_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct SkillDynamicContextExpansion {
    pub(crate) items: Vec<SkillInjection>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug)]
struct CommandRunOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    timed_out: bool,
    failure: Option<String>,
    hook_feedback: Option<String>,
}

pub(crate) async fn expand_skill_dynamic_contexts(
    skill_injections: Vec<SkillInjection>,
    mentioned_skills: &[SkillMetadata],
    skills_outcome: &SkillLoadOutcome,
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> SkillDynamicContextExpansion {
    let skill_by_path = mentioned_skills
        .iter()
        .map(|skill| (skill.path_to_skills_md.to_string_lossy().to_string(), skill))
        .collect::<HashMap<_, _>>();
    let mut expanded = Vec::with_capacity(skill_injections.len());
    let mut warnings = Vec::new();

    for mut injection in skill_injections {
        let Some(skill) = skill_by_path.get(&injection.path).copied() else {
            expanded.push(injection);
            continue;
        };
        let Some(dynamic_context) = skills_outcome.dynamic_context_for_skill(skill) else {
            expanded.push(injection);
            continue;
        };
        if !dynamic_context.inline_command_placeholders {
            expanded.push(injection);
            continue;
        }

        match expand_skill_contents(
            &injection.contents,
            skill,
            dynamic_context,
            session,
            turn_context,
        )
        .await
        {
            SkillContentsExpansion {
                contents,
                warnings: item_warnings,
            } => {
                injection.contents = contents;
                warnings.extend(item_warnings);
                expanded.push(injection);
            }
        }
    }

    SkillDynamicContextExpansion {
        items: expanded,
        warnings,
    }
}

#[derive(Debug)]
struct SkillContentsExpansion {
    contents: String,
    warnings: Vec<String>,
}

async fn expand_skill_contents(
    contents: &str,
    skill: &SkillMetadata,
    dynamic_context: &SkillDynamicContext,
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> SkillContentsExpansion {
    let placeholders = collect_inline_command_placeholders(contents);
    if placeholders.is_empty() {
        return SkillContentsExpansion {
            contents: contents.to_string(),
            warnings: Vec::new(),
        };
    }

    if !skill_allows_dynamic_context_execution(skill, turn_context) {
        return SkillContentsExpansion {
            contents: contents.to_string(),
            warnings: vec![format!(
                "Skipping dynamic context for skill `{}` because repo-scoped dynamic context requires a trusted project.",
                skill.name
            )],
        };
    }

    let Some(_turn_environment) = turn_context.environments.primary() else {
        return SkillContentsExpansion {
            contents: contents.to_string(),
            warnings: vec![format!(
                "Skipping dynamic context for skill `{}` because no execution environment is selected.",
                skill.name
            )],
        };
    };

    let max_placeholders = dynamic_context
        .max_placeholders
        .unwrap_or(DEFAULT_MAX_PLACEHOLDERS);
    let max_output_chars = dynamic_context
        .max_output_chars
        .unwrap_or(DEFAULT_MAX_OUTPUT_CHARS);
    let max_total_output_chars = dynamic_context
        .max_total_output_chars
        .unwrap_or(DEFAULT_MAX_TOTAL_OUTPUT_CHARS);
    let timeout_seconds = dynamic_context
        .timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);

    let mut replacements = Vec::new();
    let mut warnings = Vec::new();
    let mut total_output_chars = 0usize;
    let expansion_deadline =
        tokio::time::Instant::now() + Duration::from_secs(MAX_TOTAL_RUNTIME_SECONDS);

    for (index, placeholder) in placeholders.iter().enumerate() {
        if index >= max_placeholders {
            warnings.push(format!(
                "Skipped dynamic context command `{}` in skill `{}` because max_placeholders was reached.",
                placeholder.command, skill.name
            ));
            continue;
        }
        if !dynamic_context
            .allowed_commands
            .iter()
            .any(|allowed| allowed == &placeholder.command)
        {
            warnings.push(format!(
                "Skipped dynamic context command `{}` in skill `{}` because it is not allowlisted.",
                placeholder.command, skill.name
            ));
            continue;
        }

        let remaining_total = max_total_output_chars.saturating_sub(total_output_chars);
        if remaining_total == 0 {
            warnings.push(format!(
                "Skipped dynamic context command `{}` in skill `{}` because max_total_output_chars was reached.",
                placeholder.command, skill.name
            ));
            continue;
        }
        let remaining_runtime =
            expansion_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_runtime.is_zero() {
            warnings.push(format!(
                "Skipped dynamic context command `{}` in skill `{}` because the dynamic context runtime budget was reached.",
                placeholder.command, skill.name
            ));
            continue;
        }
        let command_output_limit = min(max_output_chars, remaining_total);
        let command_timeout_seconds = min(timeout_seconds, remaining_runtime.as_secs().max(1));
        match run_dynamic_context_command(
            placeholder,
            session,
            turn_context,
            command_timeout_seconds,
            command_output_limit,
        )
        .await
        {
            Ok(output) => {
                let rendered = render_command_output(&placeholder.command, &output);
                total_output_chars = total_output_chars.saturating_add(rendered.chars().count());
                if output.timed_out {
                    warnings.push(format!(
                        "Dynamic context command `{}` in skill `{}` timed out after {command_timeout_seconds}s.",
                        placeholder.command, skill.name
                    ));
                } else if let Some(failure) = output.failure.as_deref() {
                    warnings.push(format!(
                        "Dynamic context command `{}` in skill `{}` failed: {failure}",
                        placeholder.command, skill.name
                    ));
                } else if output.exit_code != Some(0) {
                    warnings.push(format!(
                        "Dynamic context command `{}` in skill `{}` exited with status {:?}.",
                        placeholder.command, skill.name, output.exit_code
                    ));
                } else if output.truncated {
                    warnings.push(format!(
                        "Dynamic context command `{}` in skill `{}` was truncated.",
                        placeholder.command, skill.name
                    ));
                }
                replacements.push((placeholder.start, placeholder.end, rendered));
            }
            Err(error) => warnings.push(format!(
                "Skipped dynamic context command `{}` in skill `{}`: {error}",
                placeholder.command, skill.name
            )),
        }
    }

    SkillContentsExpansion {
        contents: apply_replacements(contents, &replacements),
        warnings,
    }
}

fn skill_allows_dynamic_context_execution(
    skill: &SkillMetadata,
    turn_context: &TurnContext,
) -> bool {
    if skill.scope != SkillScope::Repo {
        return true;
    }

    turn_context.config.active_project.is_trusted()
}

async fn run_dynamic_context_command(
    placeholder: &InlineCommandPlaceholder,
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    timeout_seconds: u64,
    output_limit: usize,
) -> Result<CommandRunOutput, String> {
    if placeholder.command.trim().is_empty() {
        return Err("command is empty".to_string());
    }

    let Some(turn_environment) = turn_context.environments.primary() else {
        return Err("no execution environment is selected".to_string());
    };
    let tool_use_id = format!(
        "skill-dynamic-context-{}-{}",
        turn_context.sub_id,
        PROCESS_COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    run_pre_tool_use_for_command(
        session,
        turn_context,
        tool_use_id.clone(),
        &placeholder.command,
    )
    .await?;

    let session_shell = session.user_shell();
    let use_login_shell = turn_context.tools_config.allow_login_shell;
    let command = session_shell.derive_exec_args(&placeholder.command, use_login_shell);
    let mut env = create_env(
        &turn_context.shell_environment_policy,
        Some(session.conversation_id),
    );
    let dependency_env = session.dependency_env().await;
    if !dependency_env.is_empty() {
        env.extend(dependency_env.clone());
    }
    let mut explicit_env_overrides = turn_context.shell_environment_policy.r#set.clone();
    for key in dependency_env.keys() {
        if let Some(value) = env.get(key) {
            explicit_env_overrides.insert(key.clone(), value.clone());
        }
    }

    let file_system_sandbox_policy = turn_context.file_system_sandbox_policy();
    let exec_approval_requirement = session
        .services
        .exec_policy
        .create_exec_approval_requirement_for_command(ExecApprovalRequest {
            command: &command,
            approval_policy: turn_context.approval_policy.value(),
            permission_profile: turn_context.permission_profile(),
            file_system_sandbox_policy: &file_system_sandbox_policy,
            #[allow(deprecated)]
            sandbox_cwd: turn_context.cwd.as_path(),
            sandbox_permissions: SandboxPermissions::UseDefault,
            prefix_rule: None,
        })
        .await;

    let request = ShellRequest {
        command,
        shell_type: Some(session_shell.shell_type.clone()),
        hook_command: placeholder.command.clone(),
        cwd: turn_environment.cwd.clone(),
        timeout_ms: Some(timeout_seconds.saturating_mul(1000)),
        env,
        explicit_env_overrides,
        network: turn_context.network.clone(),
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        #[cfg(unix)]
        additional_permissions_preapproved: false,
        justification: Some("run allowlisted skill dynamic context command".to_string()),
        exec_approval_requirement,
    };
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn_context.clone(),
        call_id: tool_use_id.clone(),
        tool_name: ToolName::plain("shell_command"),
    };
    let mut runtime = ShellRuntime::for_shell_command(ShellRuntimeBackend::SkillDynamicContext);
    let mut orchestrator = ToolOrchestrator::new();
    let output = orchestrator
        .run(
            &mut runtime,
            &request,
            &tool_ctx,
            turn_context.as_ref(),
            turn_context.approval_policy.value(),
        )
        .await
        .map_err(tool_error_to_string)?
        .output;

    let output = apply_post_tool_use_for_command(
        session,
        turn_context,
        tool_use_id,
        &placeholder.command,
        output,
        output_limit,
    )
    .await;

    Ok(output)
}

fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn take_chars_with_truncation(text: &str, max_chars: usize) -> (String, bool) {
    let truncated = text.chars().count() > max_chars;
    (take_chars(text, max_chars), truncated)
}

async fn run_pre_tool_use_for_command(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    tool_use_id: String,
    command: &str,
) -> Result<(), String> {
    let tool_name = HookToolName::bash();
    let tool_input = serde_json::json!({ "command": command });
    match run_pre_tool_use_hooks(session, turn_context, tool_use_id, &tool_name, &tool_input).await
    {
        PreToolUseHookResult::Blocked(message) => Err(message),
        PreToolUseHookResult::Continue {
            updated_input: Some(updated_input),
        } => {
            let updated_command = updated_input.get("command").and_then(Value::as_str);
            if updated_command == Some(command) {
                Ok(())
            } else {
                Err(
                    "PreToolUse updatedInput is not supported for skill dynamic context"
                        .to_string(),
                )
            }
        }
        PreToolUseHookResult::Continue {
            updated_input: None,
        } => Ok(()),
    }
}

async fn apply_post_tool_use_for_command(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    tool_use_id: String,
    command: &str,
    output: ExecToolCallOutput,
    output_limit: usize,
) -> CommandRunOutput {
    let tool_name = HookToolName::bash();
    let tool_input = serde_json::json!({ "command": command });
    let tool_response = Value::String(format_exec_output_str(
        &output,
        turn_context.truncation_policy,
    ));
    let outcome = run_post_tool_use_hooks(
        session,
        turn_context,
        tool_use_id,
        tool_name.name().to_string(),
        tool_name.matcher_aliases().to_vec(),
        tool_input,
        tool_response,
    )
    .await;
    record_additional_contexts(session, turn_context, outcome.additional_contexts.clone()).await;

    if outcome.should_stop {
        let feedback = outcome
            .feedback_message
            .or(outcome.stop_reason)
            .unwrap_or_else(|| "PostToolUse hook stopped dynamic context output".to_string());
        return CommandRunOutput {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            failure: None,
            hook_feedback: Some(feedback),
        };
    }

    command_run_output_from_exec(output, output_limit)
}

fn command_run_output_from_exec(
    output: ExecToolCallOutput,
    output_limit: usize,
) -> CommandRunOutput {
    let (stdout, stdout_truncated) = take_chars_with_truncation(&output.stdout.text, output_limit);
    let (stderr, stderr_truncated) = take_chars_with_truncation(&output.stderr.text, output_limit);
    let stream_truncated = output.stdout.truncated_after_lines.is_some()
        || output.stderr.truncated_after_lines.is_some()
        || output.aggregated_output.truncated_after_lines.is_some();
    CommandRunOutput {
        exit_code: Some(output.exit_code),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated || stream_truncated,
        timed_out: output.timed_out,
        failure: None,
        hook_feedback: None,
    }
}

fn tool_error_to_string(error: ToolError) -> String {
    match error {
        ToolError::Rejected(message) => message,
        ToolError::Codex(error) => error.to_string(),
    }
}

fn apply_replacements(contents: &str, replacements: &[(usize, usize, String)]) -> String {
    if replacements.is_empty() {
        return contents.to_string();
    }

    let mut rendered = String::with_capacity(contents.len());
    let mut last = 0usize;
    for (start, end, replacement) in replacements {
        rendered.push_str(&contents[last..*start]);
        rendered.push_str(replacement);
        last = *end;
    }
    rendered.push_str(&contents[last..]);
    rendered
}

fn render_command_output(command: &str, output: &CommandRunOutput) -> String {
    let mut body = if let Some(feedback) = output.hook_feedback.as_deref() {
        format!("post-tool hook feedback:\n{feedback}")
    } else if output.timed_out {
        "command timed out".to_string()
    } else if let Some(failure) = output.failure.as_deref() {
        format!("command failed: {failure}")
    } else if output.exit_code == Some(0) {
        if output.stdout.is_empty() {
            "command completed with no stdout".to_string()
        } else {
            output.stdout.clone()
        }
    } else {
        let stderr = if output.stderr.is_empty() {
            "no stderr".to_string()
        } else {
            output.stderr.clone()
        };
        format!(
            "command exited with status {:?}\n{}",
            output.exit_code, stderr
        )
    };

    if output.truncated {
        body.push_str("\n[output truncated]");
    }

    format!(
        "<skill_dynamic_context command=\"{}\">\n~~~text\n{}\n~~~\n</skill_dynamic_context>",
        escape_attr(command),
        neutralize_skill_markup(&body)
    )
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn neutralize_skill_markup(value: &str) -> String {
    value
        .replace("</skill>", "<\\/skill>")
        .replace("</skill_dynamic_context>", "<\\/skill_dynamic_context>")
        .replace("~~~", "~~ ~")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_preserves_non_placeholder_text() {
        let contents = "before !`cmd` after";
        let placeholder = collect_inline_command_placeholders(contents)
            .into_iter()
            .next()
            .expect("placeholder");

        assert_eq!(
            apply_replacements(
                contents,
                &[(placeholder.start, placeholder.end, "OUT".into())]
            ),
            "before OUT after"
        );
    }

    #[test]
    fn command_output_neutralizes_skill_closing_tag() {
        let rendered = render_command_output(
            "printf",
            &CommandRunOutput {
                exit_code: Some(0),
                stdout: "</skill>".to_string(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                failure: None,
                hook_feedback: None,
            },
        );

        assert!(rendered.contains("<\\/skill>"));
        assert!(!rendered.contains("\n</skill>\n"));
    }
}

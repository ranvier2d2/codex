#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

async fn write_repo_skill(
    cwd: AbsolutePathBuf,
    fs: Arc<dyn ExecutorFileSystem>,
    name: &str,
    description: &str,
    body: &str,
) -> Result<()> {
    write_repo_skill_with_metadata(cwd, fs, name, description, body, None).await
}

async fn write_repo_skill_with_metadata(
    cwd: AbsolutePathBuf,
    fs: Arc<dyn ExecutorFileSystem>,
    name: &str,
    description: &str,
    body: &str,
    metadata: Option<&str>,
) -> Result<()> {
    let skill_dir = cwd.join(".agents").join("skills").join(name);
    fs.create_directory(
        &skill_dir,
        CreateDirectoryOptions { recursive: true },
        /*sandbox*/ None,
    )
    .await?;
    let contents = format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
    let path = skill_dir.join("SKILL.md");
    fs.write_file(&path, contents.into_bytes(), /*sandbox*/ None)
        .await?;
    if let Some(metadata) = metadata {
        let metadata_dir = skill_dir.join("agents");
        fs.create_directory(
            &metadata_dir,
            CreateDirectoryOptions { recursive: true },
            /*sandbox*/ None,
        )
        .await?;
        fs.write_file(
            &metadata_dir.join("openai.yaml"),
            metadata.as_bytes().to_vec(),
            /*sandbox*/ None,
        )
        .await?;
    }
    Ok(())
}

fn write_user_skill_with_metadata(
    home: &Path,
    name: &str,
    description: &str,
    body: &str,
    metadata: &str,
) -> Result<()> {
    let skill_dir = home.join("skills").join(name);
    fs::create_dir_all(skill_dir.join("agents"))?;
    let contents = format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
    fs::write(skill_dir.join("SKILL.md"), contents)?;
    fs::write(skill_dir.join("agents").join("openai.yaml"), metadata)?;
    Ok(())
}

fn write_blocking_user_prompt_submit_hook(home: &Path, blocked_prompt: &str) -> Result<()> {
    let script_path = home.join("block_prompt.py");
    let blocked_prompt_json = serde_json::to_string(blocked_prompt)?;
    let script = format!(
        r#"import json
import sys

payload = json.load(sys.stdin)
if payload.get("prompt") == {blocked_prompt_json}:
    print(json.dumps({{"decision": "block", "reason": "blocked by test hook"}}))
"#,
    );
    let hooks = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", script_path.display())
                }]
            }]
        }
    });

    fs::write(&script_path, script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

async fn submit_user_turn_with_skill(
    test: &TestCodex,
    prompt: &str,
    skill_name: &str,
    skill_path: PathBuf,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserTurn {
            environments: None,
            items: vec![
                UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                },
                UserInput::Skill {
                    name: skill_name.to_string(),
                    path: skill_path,
                },
            ],
            final_output_json_schema: None,
            cwd: test.config.cwd.to_path_buf(),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy,
            permission_profile,
            model: session_model,
            effort: None,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    core_test_support::wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, codex_protocol::protocol::EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_includes_skill_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let skill_body = "skill body";
    let mut builder = test_codex().with_workspace_setup(move |cwd, fs| async move {
        write_repo_skill(cwd, fs, "demo", "demo skill", skill_body).await
    });
    let test = builder.build_with_remote_env(&server).await?;

    let skill_path = test
        .config
        .cwd
        .join(".agents/skills/demo/SKILL.md")
        .canonicalize()
        .unwrap_or_else(|_| test.config.cwd.join(".agents/skills/demo/SKILL.md"))
        .to_path_buf();

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserTurn {
            environments: None,
            items: vec![
                UserInput::Text {
                    text: "please use $demo".to_string(),
                    text_elements: Vec::new(),
                },
                UserInput::Skill {
                    name: "demo".to_string(),
                    path: skill_path.clone(),
                },
            ],
            final_output_json_schema: None,
            cwd: test.config.cwd.to_path_buf(),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy,
            permission_profile,
            model: session_model,
            effort: None,
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await?;

    core_test_support::wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, codex_protocol::protocol::EventMsg::TurnComplete(_))
    })
    .await;

    let request = mock.single_request();
    let user_texts = request.message_input_texts("user");
    let skill_path_str = skill_path.to_string_lossy();
    assert!(
        user_texts.iter().any(|text| {
            text.contains("<skill>\n<name>demo</name>")
                && text.contains("<path>")
                && text.contains(skill_body)
                && text.contains(skill_path_str.as_ref())
        }),
        "expected skill instructions in user input, got {user_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_expands_skill_dynamic_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_pre_build_hook(|home| {
        let metadata = r#"
dynamic_context:
  inline_command_placeholders: true
  allowed_commands:
    - "printf dynamic-output"
"#;
        if let Err(error) = write_user_skill_with_metadata(
            home,
            "dynamic",
            "dynamic skill",
            "Dynamic value: !`printf dynamic-output`",
            metadata,
        ) {
            panic!("failed to write dynamic skill fixture: {error}");
        }
    });
    let test = builder.build(&server).await?;

    let skill_path = test
        .config
        .codex_home
        .join("skills/dynamic/SKILL.md")
        .canonicalize()
        .unwrap_or_else(|_| test.config.codex_home.join("skills/dynamic/SKILL.md"))
        .to_path_buf();

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    submit_user_turn_with_skill(&test, "please use $dynamic", "dynamic", skill_path.clone())
        .await?;

    let request = mock.single_request();
    let user_texts = request.message_input_texts("user");
    assert!(
        user_texts.iter().any(|text| {
            text.contains("<skill_dynamic_context command=\"printf dynamic-output\">")
                && text.contains("dynamic-output")
                && !text.contains("!`printf dynamic-output`")
        }),
        "expected expanded dynamic context in user input, got {user_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_skips_non_allowlisted_skill_dynamic_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_pre_build_hook(|home| {
        let metadata = r#"
dynamic_context:
  inline_command_placeholders: true
  allowed_commands:
    - "printf allowed-output"
"#;
        if let Err(error) = write_user_skill_with_metadata(
            home,
            "dynamic-blocked",
            "dynamic skill",
            "Dynamic value: !`printf blocked-output`",
            metadata,
        ) {
            panic!("failed to write dynamic skill fixture: {error}");
        }
    });
    let test = builder.build(&server).await?;

    let skill_path = test
        .config
        .codex_home
        .join("skills/dynamic-blocked/SKILL.md")
        .canonicalize()
        .unwrap_or_else(|_| {
            test.config
                .codex_home
                .join("skills/dynamic-blocked/SKILL.md")
        })
        .to_path_buf();

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    submit_user_turn_with_skill(
        &test,
        "please use $dynamic-blocked",
        "dynamic-blocked",
        skill_path,
    )
    .await?;

    let request = mock.single_request();
    let user_texts = request.message_input_texts("user");
    assert!(
        user_texts.iter().any(|text| {
            text.contains("!`printf blocked-output`")
                && !text.contains("<skill_dynamic_context")
                && !text.contains("allowed-output")
        }),
        "expected non-allowlisted placeholder to remain unexpanded, got {user_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_skips_repo_skill_dynamic_context_when_project_untrusted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_workspace_setup(move |cwd, fs| async move {
        let command = "printf repo-dynamic-output";
        let metadata = format!(
            r#"
dynamic_context:
  inline_command_placeholders: true
  allowed_commands:
    - {command:?}
"#
        );
        write_repo_skill_with_metadata(
            cwd,
            fs,
            "repo-dynamic",
            "dynamic repo skill",
            &format!("Repo dynamic value: !`{command}`"),
            Some(&metadata),
        )
        .await
    });
    let test = builder.build_with_remote_env(&server).await?;

    let skill_path = test
        .config
        .cwd
        .join(".agents/skills/repo-dynamic/SKILL.md")
        .canonicalize()
        .unwrap_or_else(|_| test.config.cwd.join(".agents/skills/repo-dynamic/SKILL.md"))
        .to_path_buf();

    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    submit_user_turn_with_skill(
        &test,
        "please use $repo-dynamic",
        "repo-dynamic",
        skill_path,
    )
    .await?;

    let request = mock.single_request();
    let user_texts = request.message_input_texts("user");
    assert!(
        user_texts.iter().any(|text| {
            text.contains("!`printf repo-dynamic-output`")
                && !text.contains("<skill_dynamic_context")
                && !text.contains("repo-dynamic-output\n~~~")
        }),
        "expected untrusted repo skill placeholder to remain unexpanded, got {user_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_prompt_submit_hook_blocks_before_skill_dynamic_context_runs() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let prompt = "please use $dynamic-side-effect";
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            let marker_path = home.join("dynamic_context_ran");
            let command = format!("printf ran > {}", marker_path.display());
            let metadata = format!(
                r#"
dynamic_context:
  inline_command_placeholders: true
  allowed_commands:
    - {command:?}
"#
            );
            let body = format!("Dynamic side effect: !`{command}`");
            if let Err(error) = write_user_skill_with_metadata(
                home,
                "dynamic-side-effect",
                "dynamic skill",
                &body,
                &metadata,
            ) {
                panic!("failed to write dynamic skill fixture: {error}");
            }
            if let Err(error) = write_blocking_user_prompt_submit_hook(home, prompt) {
                panic!("failed to write blocking hook fixture: {error}");
            }
        })
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    let skill_path = test
        .config
        .codex_home
        .join("skills/dynamic-side-effect/SKILL.md")
        .canonicalize()
        .unwrap_or_else(|_| {
            test.config
                .codex_home
                .join("skills/dynamic-side-effect/SKILL.md")
        })
        .to_path_buf();

    submit_user_turn_with_skill(&test, prompt, "dynamic-side-effect", skill_path).await?;

    assert!(
        !test.config.codex_home.join("dynamic_context_ran").exists(),
        "dynamic context command should not run after UserPromptSubmit blocks the turn"
    );

    Ok(())
}

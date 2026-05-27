use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::tools::handlers::parse_arguments;
use crate::tools::js_repl::JS_REPL_PRAGMA_PREFIX;
use crate::tools::js_repl::JsReplArgs;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_features::Feature;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::protocol::ExecCommandSource;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub struct JsReplHandler;
pub struct JsReplResetHandler;

fn join_outputs(stdout: &str, stderr: &str) -> String {
    if stdout.is_empty() {
        stderr.to_string()
    } else if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    }
}

fn build_js_repl_exec_output(
    output: &str,
    error: Option<&str>,
    duration: Duration,
) -> ExecToolCallOutput {
    let stdout = output.to_string();
    let stderr = error.unwrap_or("").to_string();
    let aggregated_output = join_outputs(&stdout, &stderr);
    ExecToolCallOutput {
        exit_code: if error.is_some() { 1 } else { 0 },
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(stderr),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    }
}

async fn emit_js_repl_exec_begin(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    cwd: codex_utils_absolute_path::AbsolutePathBuf,
) {
    let emitter = ToolEmitter::unified_exec(
        &["js_repl".to_string()],
        codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        ExecCommandSource::Agent,
        /*process_id*/ None,
        /*plugin_attribution*/ None,
    );
    let ctx = ToolEventCtx::new(session, turn, call_id, /*turn_diff_tracker*/ None);
    emitter.emit(ctx, ToolEventStage::Begin).await;
}

async fn emit_js_repl_exec_end(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    cwd: codex_utils_absolute_path::AbsolutePathBuf,
    output: &str,
    error: Option<&str>,
    duration: Duration,
) {
    let exec_output = build_js_repl_exec_output(output, error, duration);
    let emitter = ToolEmitter::unified_exec(
        &["js_repl".to_string()],
        codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        ExecCommandSource::Agent,
        /*process_id*/ None,
        /*plugin_attribution*/ None,
    );
    let ctx = ToolEventCtx::new(session, turn, call_id, /*turn_diff_tracker*/ None);
    let stage = if error.is_some() {
        ToolEventStage::Failure(ToolEventFailure::Output(exec_output))
    } else {
        ToolEventStage::Success {
            output: exec_output,
            applied_patch_delta: None,
        }
    };
    emitter.emit(ctx, stage).await;
}
impl ToolExecutor<ToolInvocation> for JsReplHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("js_repl")
    }

    fn spec(&self) -> ToolSpec {
        codex_tools::create_js_repl_tool()
    }

    fn exposure(&self) -> codex_tools::ToolExposure {
        codex_tools::ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                step_context,
                cancellation_token,
                tracker,
                payload,
                call_id,
                ..
            } = invocation;

            if !session.features().enabled(Feature::JsRepl) {
                return Err(FunctionCallError::RespondToModel(
                    "js_repl is disabled by feature flag".to_string(),
                ));
            }

            let args = match payload {
                ToolPayload::Function { arguments } => parse_arguments(&arguments)?,
                ToolPayload::Custom { input } => parse_freeform_args(&input)?,
                _ => {
                    return Err(FunctionCallError::RespondToModel(
                        "js_repl expects custom or function payload".to_string(),
                    ));
                }
            };
            let manager = turn.js_repl.manager().await?;
            let cwd = step_context
                .environments
                .primary()
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "js_repl is unavailable without a ready environment".to_string(),
                    )
                })?
                .cwd()
                .to_abs_path()
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "js_repl requires a local working directory: {err}"
                    ))
                })?;
            let started_at = Instant::now();
            emit_js_repl_exec_begin(session.as_ref(), turn.as_ref(), &call_id, cwd.clone()).await;
            let result = manager
                .execute_with_cancellation(
                    Arc::clone(&session),
                    step_context,
                    cancellation_token,
                    tracker,
                    args,
                )
                .await;
            let result = match result {
                Ok(result) => result,
                Err(err) => {
                    let message = err.to_string();
                    emit_js_repl_exec_end(
                        session.as_ref(),
                        turn.as_ref(),
                        &call_id,
                        cwd,
                        "",
                        Some(&message),
                        started_at.elapsed(),
                    )
                    .await;
                    return Err(err);
                }
            };

            let content = result.output;
            let mut items = Vec::with_capacity(result.content_items.len() + 1);
            if !content.is_empty() {
                items.push(FunctionCallOutputContentItem::InputText {
                    text: content.clone(),
                });
            }
            items.extend(result.content_items);

            emit_js_repl_exec_end(
                session.as_ref(),
                turn.as_ref(),
                &call_id,
                cwd,
                &content,
                /*error*/ None,
                started_at.elapsed(),
            )
            .await;

            if items.is_empty() {
                Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    content,
                    Some(true),
                )))
            } else {
                Ok(boxed_tool_output(FunctionToolOutput::from_content(
                    items,
                    Some(true),
                )))
            }
        })
    }
}

impl CoreToolRuntime for JsReplHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::Custom { .. }
        )
    }
}

impl ToolExecutor<ToolInvocation> for JsReplResetHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("js_repl_reset")
    }

    fn spec(&self) -> ToolSpec {
        codex_tools::create_js_repl_reset_tool()
    }

    fn exposure(&self) -> codex_tools::ToolExposure {
        codex_tools::ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            if !invocation.session.features().enabled(Feature::JsRepl) {
                return Err(FunctionCallError::RespondToModel(
                    "js_repl is disabled by feature flag".to_string(),
                ));
            }
            let manager = invocation.turn.js_repl.manager().await?;
            manager.reset().await?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                "js_repl kernel reset".to_string(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for JsReplResetHandler {}

fn parse_freeform_args(input: &str) -> Result<JsReplArgs, FunctionCallError> {
    if input.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "js_repl expects raw JavaScript tool input (non-empty). Provide JS source text, optionally with first-line `// codex-js-repl: ...`."
                .to_string(),
        ));
    }

    let mut args = JsReplArgs {
        code: input.to_string(),
        timeout_ms: None,
    };

    let mut lines = input.splitn(2, '\n');
    let first_line = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let trimmed = first_line.trim_start();
    let Some(pragma) = trimmed.strip_prefix(JS_REPL_PRAGMA_PREFIX) else {
        reject_json_or_quoted_source(&args.code)?;
        return Ok(args);
    };

    let mut timeout_ms: Option<u64> = None;
    let directive = pragma.trim();
    if !directive.is_empty() {
        for token in directive.split_whitespace() {
            let (key, value) = token.split_once('=').ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "js_repl pragma expects space-separated key=value pairs (supported keys: timeout_ms); got `{token}`"
                ))
            })?;
            match key {
                "timeout_ms" => {
                    if timeout_ms.is_some() {
                        return Err(FunctionCallError::RespondToModel(
                            "js_repl pragma specifies timeout_ms more than once".to_string(),
                        ));
                    }
                    let parsed = value.parse::<u64>().map_err(|_| {
                        FunctionCallError::RespondToModel(format!(
                            "js_repl pragma timeout_ms must be an integer; got `{value}`"
                        ))
                    })?;
                    timeout_ms = Some(parsed);
                }
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "js_repl pragma only supports timeout_ms; got `{key}`"
                    )));
                }
            }
        }
    }

    if rest.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "js_repl pragma must be followed by JavaScript source on subsequent lines".to_string(),
        ));
    }

    reject_json_or_quoted_source(rest)?;
    args.code = rest.to_string();
    args.timeout_ms = timeout_ms;
    Ok(args)
}

fn reject_json_or_quoted_source(code: &str) -> Result<(), FunctionCallError> {
    let trimmed = code.trim();
    if trimmed.starts_with("```") {
        return Err(FunctionCallError::RespondToModel(
            "js_repl expects raw JavaScript source, not markdown code fences. Resend plain JS only (optional first line `// codex-js-repl: ...`)."
                .to_string(),
        ));
    }
    let Ok(value) = serde_json::from_str::<JsonValue>(trimmed) else {
        return Ok(());
    };
    match value {
        JsonValue::Object(_) | JsonValue::String(_) => Err(FunctionCallError::RespondToModel(
            "js_repl is a freeform tool and expects raw JavaScript source. Resend plain JS only (optional first line `// codex-js-repl: ...`); do not send JSON (`{\"code\":...}`), quoted code, or markdown fences."
                .to_string(),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "js_repl_tests.rs"]
mod tests;

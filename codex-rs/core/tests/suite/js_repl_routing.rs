use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_features::Feature;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::FunctionCallError;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use codex_tools::ToolSpec;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call_with_namespace;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct StepCustomTool {
    generation: usize,
    generations: Arc<AtomicUsize>,
}

impl ToolContributor for StepCustomTool {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        Vec::new()
    }

    fn tools_for_step(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
        _step_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![Arc::new(Self {
            generation: self.generations.fetch_add(1, Ordering::Relaxed) + 1,
            generations: Arc::clone(&self.generations),
        })]
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for StepCustomTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("editor", "echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "editor".to_string(),
            description: "Editor tools.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Custom(FreeformTool {
                name: "echo".to_string(),
                description: format!("Step {}", self.generation),
                defer_loading: None,
                format: FreeformToolFormat {
                    r#type: "grammar".to_string(),
                    syntax: "lark".to_string(),
                    definition: "start: /.+/".to_string(),
                },
            })],
        })
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Custom { input } = call.payload else {
                return Err(FunctionCallError::Fatal(
                    "expected custom payload".to_string(),
                ));
            };
            Ok(Box::new(JsonToolOutput::new(serde_json::json!({
                "description": format!("Step {}", self.generation),
                "input": input,
            }))) as Box<dyn ToolOutput>)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_repl_dispatches_namespaced_custom_tools_from_the_captured_step() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "js_repl runs in a local Node process");
    let server = responses::start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(StepCustomTool {
        generation: 0,
        generations: Arc::new(AtomicUsize::new(0)),
    }));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::JsRepl)
                .expect("enable test feature");
        });
    let test = builder.build_with_auto_env(&server).await?;
    let initial = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call_with_namespace(
                "nested",
                "functions",
                "js_repl",
                r#"
var first = await codex.tool('editor__echo', 'raw patch');
var second = await codex.tool('editorecho', 'raw patch');
console.log(JSON.stringify([JSON.parse(first.output), JSON.parse(second.output)]));
"#,
            ),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;
    test.submit_turn("Invoke the custom tool through both supported namespace aliases")
        .await?;

    let body = initial.single_request().body_json();
    let tool = body["tools"]
        .as_array()
        .expect("request tools")
        .iter()
        .find(|tool| tool["name"] == "editor")
        .expect("editor namespace");
    let expected = serde_json::json!({
        "description": tool["tools"][0]["description"],
        "input": "raw patch",
    });
    let (output, _) = final_mock
        .single_request()
        .custom_tool_call_output_content_and_success("nested")
        .expect("custom tool output");
    let output = output.expect("output text");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output)
            .with_context(|| format!("invalid nested tool output: {output}"))?,
        serde_json::json!([expected, expected])
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_repl_remains_direct_and_persistent_in_code_mode_only() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "js_repl runs in a local Node process");
    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::JsRepl)
            .expect("enable test feature");
        config
            .features
            .enable(Feature::CodeMode)
            .expect("enable test feature");
        config
            .features
            .enable(Feature::CodeModeOnly)
            .expect("enable test feature");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let initial = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call_with_namespace(
                "first",
                "functions",
                "js_repl",
                "var answer = 40; console.log(answer);",
            ),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call_with_namespace(
                "second",
                "functions",
                "js_repl",
                "console.log(answer + 2);",
            ),
            ev_completed("resp-2"),
        ]),
    )
    .await;
    let final_mock = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("resp-3"),
        ]),
    )
    .await;

    test.submit_turn("Reuse the JavaScript kernel in two steps")
        .await?;
    let body = initial.single_request().body_json();
    let tools = body["tools"].as_array().expect("request tools");
    for name in ["js_repl", "js_repl_reset"] {
        assert!(
            tools.iter().any(|tool| {
                tool["name"] == name
                    || tool["tools"]
                        .as_array()
                        .is_some_and(|children| children.iter().any(|child| child["name"] == name))
            }),
            "missing direct tool {name}: {tools:?}"
        );
    }
    let (output, success) = final_mock
        .single_request()
        .custom_tool_call_output_content_and_success("second")
        .expect("custom tool output");
    assert_eq!((output, success), (Some("42".to_string()), None));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_repl_tools_only_routes_default_namespace_calls() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "js_repl runs in a local Node process");
    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::JsRepl)
            .expect("enable test feature");
        config
            .features
            .enable(Feature::JsReplToolsOnly)
            .expect("enable test feature");
        config.update_plan_enabled = true;
    });
    let test = builder.build_with_auto_env(&server).await?;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_function_call_with_namespace("direct", "functions", "update_plan", "{\"plan\":[]}"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let rejected = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call_with_namespace(
                "nested", "functions", "js_repl",
                "var result = await codex.tool('update_plan', {plan: []}); console.log(result.type);",
            ),
            ev_completed("resp-2"),
        ]),
    ).await;
    let final_mock = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("resp-3"),
        ]),
    )
    .await;

    test.submit_turn("Use js_repl instead of direct tools")
        .await?;
    let (output, success) = rejected
        .single_request()
        .function_call_output_content_and_success("direct")
        .expect("function tool output");
    assert_eq!(
        (output, success),
        (
            Some(
                "direct tool calls are disabled; use js_repl and codex.tool(...) instead"
                    .to_string()
            ),
            None,
        )
    );
    let (output, success) = final_mock
        .single_request()
        .custom_tool_call_output_content_and_success("nested")
        .expect("custom tool output");
    assert_eq!(
        (output, success),
        (Some("function_call_output".to_string()), None)
    );
    Ok(())
}

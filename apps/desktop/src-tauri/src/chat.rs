//! 与 DeepSeek 模型对话的桥接层：带退避重试的补全 / 流式工具轮、同族备用模型切换、
//! 工具调用兜底解析、ChatMessage ↔ wire 消息互转。
//!
//! 从 main.rs 抽出（Plan16）：纯代码搬移，无行为变更。

use novacode_deepseek_client::{
    chat_completion, chat_stream_with_tools, parse_dsml_tool_calls, ChatMessage, ToolCall,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

/// 带退避重试的 chat_completion，容忍偶发网络抖动 / 5xx / 限流，避免一次瞬时失败打断整轮长任务。
///
/// 可重试错误（网络收发失败、服务端错误、429）按 0.5s→1s→2s 退避重试，最多 4 次；
/// 确定性错误（认证、余额、参数、上下文超限）立即返回不重试。重试时向前端推送提示。
pub(crate) async fn chat_completion_with_retry(
    base_url: &str,
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Option<Vec<serde_json::Value>>,
    app: &AppHandle,
) -> Result<novacode_deepseek_client::ChatCompletionResult, String> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match chat_completion(base_url, api_key, messages.clone(), model, tools.clone()).await {
            Ok(result) => return Ok(result),
            Err(err) if err.is_retryable() && attempt < MAX_ATTEMPTS => {
                let delay_ms = 500u64 * 2u64.pow(attempt - 1);
                let _ = app.emit(
                    "chat-chunk",
                    format!("\n\n（网络波动，正在重试（第 {attempt}/{} 次）…）\n\n", MAX_ATTEMPTS - 1),
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// 带退避重试的流式工具轮：边流边把叙述 token 推到前端（"chat-chunk"），同时累积 tool_calls。
///
/// 仅在「尚未外显任何内容」时才对可重试错误重试，避免重试导致内容重复外显；
/// 一旦已开始流式输出再失败，则直接返回错误。
/// 返回同族备用模型：flash↔pro 互为 fallback，持续失败（如 overload）时切换求生。
pub(crate) fn fallback_model_for(model: &str) -> Option<&'static str> {
    match model {
        "deepseek-v4-flash" => Some("deepseek-v4-pro"),
        "deepseek-v4-pro" => Some("deepseek-v4-flash"),
        _ => None,
    }
}

pub(crate) async fn stream_round_with_retry(
    base_url: &str,
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Vec<serde_json::Value>,
    app: &AppHandle,
) -> Result<novacode_deepseek_client::ChatCompletionResult, String> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut model = model.to_string();
    let mut fallback_used = false;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let emitted = Arc::new(AtomicBool::new(false));
        let emitted_cb = emitted.clone();
        let app_cb = app.clone();
        let result = chat_stream_with_tools(
            base_url,
            api_key,
            messages.clone(),
            &model,
            tools.clone(),
            move |chunk| {
                emitted_cb.store(true, Ordering::SeqCst);
                let _ = app_cb.emit("chat-chunk", chunk);
            },
        )
        .await;
        match result {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && !emitted.load(Ordering::SeqCst) => {
                if attempt < MAX_ATTEMPTS {
                    let delay_ms = 500u64 * 2u64.pow(attempt - 1);
                    let _ = app.emit(
                        "chat-chunk",
                        format!("\n\n（网络波动，正在重试（第 {attempt}/{} 次）…）\n\n", MAX_ATTEMPTS - 1),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                } else if !fallback_used {
                    // 主模型多次失败（overload/限流）：切同族备用模型再试一轮。
                    match fallback_model_for(&model) {
                        Some(fb) => {
                            let _ = app.emit(
                                "chat-chunk",
                                format!("\n\n（主模型持续不可用，已切换备用模型 {fb} 重试…）\n\n"),
                            );
                            model = fb.to_string();
                            fallback_used = true;
                            attempt = 0;
                        }
                        None => return Err(err.to_string()),
                    }
                } else {
                    return Err(err.to_string());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(crate) fn recover_tool_calls_from_content(content: &str) -> Vec<ToolCall> {
    parse_dsml_tool_calls(content)
}

pub(crate) fn assistant_message_for_tool_calls(
    assistant_message: serde_json::Value,
    tool_calls: &[ToolCall],
) -> serde_json::Value {
    if assistant_message
        .get("tool_calls")
        .and_then(|calls| calls.as_array())
        .is_some()
    {
        return assistant_message;
    }

    serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": tool_calls
    })
}

pub(crate) fn chat_messages_to_wire(messages: Vec<ChatMessage>) -> Vec<serde_json::Value> {
    messages
        .into_iter()
        .map(|message| {
            serde_json::json!({
                "role": message.role,
                "content": message.content,
            })
        })
        .collect()
}

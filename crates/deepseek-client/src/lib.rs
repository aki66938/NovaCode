use futures_util::StreamExt;
use novacode_shared::{ApiKeyStatus, RawUsage};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeepSeekError {
    #[error("DEEPSEEK_API_KEY 未配置")]
    MissingApiKey,
    #[error("认证失败，请检查 API Key 是否正确")]
    Unauthorized,
    #[error("余额不足，请前往 DeepSeek 平台充值")]
    InsufficientBalance,
    #[error("请求被限流，请稍后重试")]
    RateLimited,
    #[error("请求参数错误: {0}")]
    BadRequest(String),
    #[error("上下文长度超限")]
    ContextLengthExceeded,
    #[error("服务端错误，请稍后重试")]
    ServerError,
    #[error("网络连接失败: {0}")]
    Http(#[from] reqwest::Error),
}

impl DeepSeekError {
    /// 判断该错误是否为瞬时、可重试的错误。
    ///
    /// 网络收发失败、服务端 5xx、限流 429 属于可重试；认证失败、余额不足、参数错误、
    /// 上下文超限属于确定性错误，重试无意义。用于让长任务的 Agent 循环容忍偶发网络抖动。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DeepSeekError::Http(_) | DeepSeekError::ServerError | DeepSeekError::RateLimited
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionCall,
}

#[derive(Clone, Debug)]
pub struct ChatCompletionResult {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub assistant_message: serde_json::Value,
    pub usage: Option<RawUsage>,
}

/// 检测当前进程是否能读取 DeepSeek API Key。
///
/// 输入环境变量读取闭包，输出脱敏后的 API Key 状态；
/// 本方法不返回、不记录、不持久化 Key 明文。
pub fn detect_api_key_status(read_env: impl FnOnce(&str) -> Option<String>) -> ApiKeyStatus {
    match read_env("DEEPSEEK_API_KEY") {
        Some(v) if !v.trim().is_empty() => ApiKeyStatus::Configured,
        _ => ApiKeyStatus::Missing,
    }
}

/// 将 HTTP 状态码和响应体映射为可理解的 DeepSeekError。
///
/// 输入状态码和原始响应文本，输出分类错误；
/// 本方法不重试，重试策略由调用方决定。
fn classify_api_error(status: u16, body: &str) -> DeepSeekError {
    match status {
        401 => DeepSeekError::Unauthorized,
        402 => DeepSeekError::InsufficientBalance,
        429 => DeepSeekError::RateLimited,
        400 => {
            // 上下文超限会走 400
            if body.contains("context") || body.contains("length") {
                DeepSeekError::ContextLengthExceeded
            } else {
                DeepSeekError::BadRequest(body.chars().take(200).collect())
            }
        }
        500..=599 => DeepSeekError::ServerError,
        _ => DeepSeekError::BadRequest(body.chars().take(200).collect()),
    }
}

/// 向 DeepSeek API 发起流式聊天请求，通过回调逐块推送内容，完成后返回原始 usage。
///
/// 输入 API Key、消息列表和模型名；每收到内容 chunk 调用一次 on_chunk；
/// 流结束后返回服务端原始 usage（若缺失则返回 None）；
/// 错误时流中断，on_chunk 不再被调用。
pub async fn chat_stream<F>(
    api_key: &str,
    messages: Vec<ChatMessage>,
    model: &str,
    on_chunk: F,
) -> Result<Option<RawUsage>, DeepSeekError>
where
    F: Fn(String),
{
    // 流式请求只限连接超时，不设总超时（长回复的流式读取可能远超固定时长）。
    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(DeepSeekError::Http)?;

    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true }
    });

    let response = client
        .post("https://api.deepseek.com/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(classify_api_error(status, &body));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut captured_usage: Option<RawUsage> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // 逐行解析 SSE，每条完整行单独处理
        loop {
            match buffer.find('\n') {
                None => break,
                Some(pos) => {
                    let line = buffer[..pos].trim().to_string();
                    buffer = buffer[pos + 1..].to_string();

                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    if data == "[DONE]" {
                        return Ok(captured_usage);
                    }

                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };

                    // 末尾 usage chunk（stream_options.include_usage）：choices 为空数组
                    if let Some(usage_val) = value.get("usage") {
                        if !usage_val.is_null() {
                            captured_usage = Some(parse_raw_usage(usage_val, data));
                        }
                    }

                    // 内容 chunk：从 choices[0].delta.content 取文本
                    if let Some(content) = value
                        .pointer("/choices/0/delta/content")
                        .and_then(|v| v.as_str())
                    {
                        if !content.is_empty() {
                            on_chunk(content.to_string());
                        }
                    }
                }
            }
        }
    }

    Ok(captured_usage)
}

/// 查找正文中工具调用标记（DSML / ToolCall 各变体）的最早字节位置；无标记返回 None。
fn tool_markup_index(content: &str) -> Option<usize> {
    let stripped = strip_dsml_bars(content);
    // 在去竖线后的串里找标记，再映射回原串大致位置不可靠；改为直接在原串找各变体前缀。
    ["<DSML", "<ToolCall", "<\u{FF5C}"]
        .iter()
        .filter_map(|m| content.find(m))
        .min()
        .or_else(|| {
            // 去竖线后才暴露的 <DSML（原串是 <｜DSML）：用 stripped 命中则保守地从首个 '<' 截断
            if stripped.contains("<DSML") || stripped.contains("<ToolCall") {
                content.find('<')
            } else {
                None
            }
        })
}

/// 带工具的流式聊天：边流边把**安全的**叙述内容通过回调推送（token 级），
/// 同时累积 delta.tool_calls，结束后返回结构化结果。
///
/// 内置防泄漏守卫：一旦检测到正文出现工具调用标记（DSML / `<ToolCall>`），停止外显后续内容，
/// 把整段标记留给上层 DSML 兜底解析，避免标记 token 流到界面。完整 content 仍在返回值里供解析。
pub async fn chat_stream_with_tools<F>(
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Vec<serde_json::Value>,
    on_content: F,
) -> Result<ChatCompletionResult, DeepSeekError>
where
    F: Fn(String),
{
    const GUARD: usize = 12; // 末尾保留窗口，防止正在形成的标记被提前外显
    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(DeepSeekError::Http)?;
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::json!("auto");
    }

    let response = client
        .post("https://api.deepseek.com/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(classify_api_error(status, &text));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut content_full = String::new();
    let mut emitted_bytes = 0usize; // 已外显的 content 字节数
    let mut leaked = false;
    let mut usage: Option<RawUsage> = None;
    // tool_calls 累积：按 index 收集 id / name / arguments 片段
    let mut tool_acc: Vec<(String, String, String)> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        loop {
            let Some(pos) = buffer.find('\n') else { break };
            let line = buffer[..pos].trim().to_string();
            buffer = buffer[pos + 1..].to_string();
            let Some(data) = line.strip_prefix("data: ") else { continue };
            if data == "[DONE]" {
                break;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            if let Some(usage_val) = value.get("usage") {
                if !usage_val.is_null() {
                    usage = Some(parse_raw_usage(usage_val, data));
                }
            }
            // 累积 tool_calls 片段
            if let Some(calls) = value
                .pointer("/choices/0/delta/tool_calls")
                .and_then(|v| v.as_array())
            {
                for call in calls {
                    let idx = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    while tool_acc.len() <= idx {
                        tool_acc.push((String::new(), String::new(), String::new()));
                    }
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            tool_acc[idx].0 = id.to_string();
                        }
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            tool_acc[idx].1 = name.to_string();
                        }
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                        tool_acc[idx].2.push_str(args);
                    }
                }
            }
            // 内容片段：守卫式外显
            if let Some(delta) = value
                .pointer("/choices/0/delta/content")
                .and_then(|v| v.as_str())
            {
                if !delta.is_empty() {
                    content_full.push_str(delta);
                    if !leaked {
                        if let Some(marker) = tool_markup_index(&content_full) {
                            // 命中标记：外显到标记前，之后停止外显
                            if marker > emitted_bytes {
                                on_content(content_full[emitted_bytes..marker].to_string());
                            }
                            emitted_bytes = content_full.len();
                            leaked = true;
                        } else {
                            // 未命中：外显到 len-GUARD（在字符边界上）
                            let safe_end = content_full.len().saturating_sub(GUARD);
                            if safe_end > emitted_bytes && content_full.is_char_boundary(safe_end) {
                                on_content(content_full[emitted_bytes..safe_end].to_string());
                                emitted_bytes = safe_end;
                            }
                        }
                    }
                }
            }
        }
    }
    // 收尾：未泄漏时把保留窗口里的安全尾巴补发
    if !leaked && content_full.len() > emitted_bytes {
        on_content(content_full[emitted_bytes..].to_string());
    }

    let tool_calls: Vec<ToolCall> = tool_acc
        .into_iter()
        .enumerate()
        .filter(|(_, (_, name, _))| !name.is_empty())
        .map(|(i, (id, name, args))| ToolCall {
            id: if id.is_empty() { format!("call_{i}") } else { id },
            kind: "function".to_string(),
            function: ToolFunctionCall { name, arguments: args },
        })
        .collect();

    let assistant_message = serde_json::json!({
        "role": "assistant",
        "content": if content_full.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(content_full.clone()) },
        "tool_calls": tool_calls,
    });

    Ok(ChatCompletionResult {
        content: if content_full.is_empty() { None } else { Some(content_full) },
        tool_calls,
        assistant_message,
        usage,
    })
}

/// 向 DeepSeek API 发起一次非流式聊天请求，可携带 tool schema。
///
/// 输入 API Key、消息 JSON、模型名和可选工具列表；输出 assistant 内容、tool calls 和 usage。
/// 本方法不执行工具，只负责解析模型意图。
pub async fn chat_completion(
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Option<Vec<serde_json::Value>>,
) -> Result<ChatCompletionResult, DeepSeekError> {
    // 必须设置总超时：服务端 hang 时若无超时会无限等待，前端表现为「正在思考」永不出 token。
    // 超时映射为可重试的 Http 错误，由上层退避重试。
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(DeepSeekError::Http)?;
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": false
    });

    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::json!("auto");
    }

    let response = client
        .post("https://api.deepseek.com/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(classify_api_error(status, &body));
    }

    let value = response.json::<serde_json::Value>().await?;
    parse_chat_completion_response(&value)
}

/// 单一币种的余额明细（金额为字符串，保留服务端原始精度）。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceInfo {
    pub currency: String,
    pub total_balance: String,
    pub granted_balance: String,
    pub topped_up_balance: String,
}

/// 账户余额信息（DeepSeek 公开账户接口仅提供余额，无更多用户资料）。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBalance {
    pub is_available: bool,
    pub balance_infos: Vec<BalanceInfo>,
}

/// 查询 DeepSeek 账户余额。
///
/// 输入 API Key；GET /user/balance（Bearer 认证），返回可用状态与各币种余额明细。
/// 这是 DeepSeek 公开 API 唯一的账户信息接口，没有用户名等更多资料。
pub async fn get_user_balance(api_key: &str) -> Result<UserBalance, DeepSeekError> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(DeepSeekError::Http)?;
    let response = client
        .get("https://api.deepseek.com/user/balance")
        .bearer_auth(api_key)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(classify_api_error(status, &body));
    }
    let value = response.json::<serde_json::Value>().await?;
    parse_user_balance(&value)
}

/// 把 /user/balance 的原始 JSON（snake_case）解析为 UserBalance，缺失字段安全降级。
fn parse_user_balance(value: &serde_json::Value) -> Result<UserBalance, DeepSeekError> {
    let is_available = value
        .get("is_available")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let balance_infos = value
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| BalanceInfo {
                    currency: item.get("currency").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    total_balance: item.get("total_balance").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                    granted_balance: item.get("granted_balance").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                    topped_up_balance: item.get("topped_up_balance").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(UserBalance { is_available, balance_infos })
}

/// 将服务端 usage JSON 解析为 RawUsage。
///
/// 输入 usage 字段的 serde_json::Value 和原始 JSON 字符串，输出标准化结构；
/// 缺失字段保留为 0，raw_json 保存完整原始字符串供审计。
fn parse_raw_usage(usage: &serde_json::Value, raw_data: &str) -> RawUsage {
    RawUsage {
        prompt_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        total_tokens: usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        prompt_cache_hit_tokens: usage
            .get("prompt_cache_hit_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_miss_tokens: usage
            .get("prompt_cache_miss_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        raw_json: raw_data.to_string(),
    }
}

fn parse_chat_completion_response(
    value: &serde_json::Value,
) -> Result<ChatCompletionResult, DeepSeekError> {
    let assistant_message = value
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| DeepSeekError::BadRequest("响应缺少 choices[0].message".to_string()))?;
    let content = assistant_message
        .get("content")
        .and_then(|content| content.as_str())
        .map(str::to_string);
    let tool_calls = assistant_message
        .get("tool_calls")
        .cloned()
        .map(serde_json::from_value::<Vec<ToolCall>>)
        .transpose()
        .map_err(|err| DeepSeekError::BadRequest(err.to_string()))?
        .unwrap_or_default();
    let usage = value
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(|usage| parse_raw_usage(usage, &value.to_string()));

    Ok(ChatCompletionResult {
        content,
        tool_calls,
        assistant_message,
        usage,
    })
}

/// 从 DeepSeek 泄漏到正文的 DSML 工具标记中恢复 tool calls。
///
/// 输入 assistant 文本内容，输出可交给 Host 执行的 ToolCall 列表。DeepSeek 有时不会返回
/// OpenAI-compatible `tool_calls` 字段，而是把 DSML 标记直接输出到正文。不同模型版本会用
/// 不同数量的全角竖线（`｜DSML｜` 或 `｜｜DSML｜｜`），因此先归一化掉所有竖线再解析，
/// 让解析对竖线数量保持容忍。
///
/// 注意：归一化会移除正文中所有全角竖线 U+FF5C；DSML 兜底路径下文件路径/内容极少包含该字符，
/// 这是可接受的取舍。原生 tool_calls 路径不受影响。
pub fn parse_dsml_tool_calls(raw_content: &str) -> Vec<ToolCall> {
    let content = strip_dsml_bars(raw_content);
    // 先尝试 DSML 变体；没有命中时再尝试 XML 风格的 <ToolCall name="..."> 变体
    //（实测 DeepSeek 偶尔会输出这种格式，工具名/参数同样可恢复执行）。
    let calls = parse_markup_tool_calls(
        &content,
        "<DSMLinvoke name=\"",
        "</DSMLinvoke>",
        "<DSMLparameter name=\"",
        "</DSMLparameter>",
    );
    if !calls.is_empty() {
        return calls;
    }
    parse_markup_tool_calls(
        &content,
        "<ToolCall name=\"",
        "</ToolCall>",
        "<parameter name=\"",
        "</parameter>",
    )
}

/// 通用的「标记式工具调用」解析器：按给定的 invoke/parameter 起止标记从正文恢复 ToolCall。
fn parse_markup_tool_calls(
    content: &str,
    invoke_marker: &str,
    invoke_end: &str,
    param_marker: &str,
    param_end: &str,
) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut cursor = 0;

    while let Some(offset) = content[cursor..].find(invoke_marker) {
        let start = cursor + offset + invoke_marker.len();
        let Some(name_end_offset) = content[start..].find('"') else {
            break;
        };
        let name = &content[start..start + name_end_offset];
        let Some(open_end_offset) = content[start + name_end_offset..].find('>') else {
            break;
        };
        let body_start = start + name_end_offset + open_end_offset + 1;
        let Some(body_end_offset) = content[body_start..].find(invoke_end) else {
            break;
        };
        let body_end = body_start + body_end_offset;
        let params =
            parse_markup_parameters(&content[body_start..body_end], param_marker, param_end);
        let Ok(arguments) = serde_json::to_string(&params) else {
            cursor = body_end + invoke_end.len();
            continue;
        };

        calls.push(ToolCall {
            id: format!("dsml_call_{}", calls.len()),
            kind: "function".to_string(),
            function: ToolFunctionCall {
                name: name.to_string(),
                arguments,
            },
        });
        cursor = body_end + invoke_end.len();
    }

    calls
}

/// 移除全角竖线 U+FF5C，使 `<｜DSML｜...>` 与 `<｜｜DSML｜｜...>` 归一化为 `<DSML...>`。
fn strip_dsml_bars(content: &str) -> String {
    content.replace('\u{FF5C}', "")
}

/// 从将要展示给用户的正文中清除工具调用标记（DSML 与 <ToolCall> 两种变体），避免泄漏成可见文本。
///
/// DeepSeek 偶尔把工具调用直接吐进正文（尤其在不带 tools 的收尾请求里）。工具调用
/// 总是出现在叙述之后，因此从第一个标记处截断，保留前面的自然语言叙述，丢弃整段标记。
/// 正文中若没有标记则原样返回。
pub fn strip_dsml_markup(content: &str) -> String {
    let normalized = strip_dsml_bars(content);
    let cut = [normalized.find("<DSML"), normalized.find("<ToolCall")]
        .into_iter()
        .flatten()
        .min();
    match cut {
        Some(pos) => normalized[..pos].trim_end().to_string(),
        None => content.to_string(),
    }
}

fn parse_markup_parameters(
    body: &str,
    param_marker: &str,
    param_end: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut params = serde_json::Map::new();
    let mut cursor = 0;

    while let Some(offset) = body[cursor..].find(param_marker) {
        let start = cursor + offset + param_marker.len();
        let Some(name_end_offset) = body[start..].find('"') else {
            break;
        };
        let name = &body[start..start + name_end_offset];
        let Some(open_end_offset) = body[start + name_end_offset..].find('>') else {
            break;
        };
        let value_start = start + name_end_offset + open_end_offset + 1;
        let Some(value_end_offset) = body[value_start..].find(param_end) else {
            break;
        };
        let value_end = value_start + value_end_offset;
        let value = normalize_dsml_parameter(name, &body[value_start..value_end]);
        params.insert(name.to_string(), serde_json::Value::String(value));
        cursor = value_end + param_end.len();
    }

    params
}

fn normalize_dsml_parameter(name: &str, value: &str) -> String {
    let trimmed = value.trim();
    if name == "path" {
        trimmed.trim_start_matches(['\\', '/']).to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_call_completion_response() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "create_file",
                            "arguments": "{\"path\":\"test.txt\",\"content\":\"\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_cache_hit_tokens": 0,
                "prompt_cache_miss_tokens": 10
            }
        });

        let parsed = parse_chat_completion_response(&raw).expect("response should parse");

        assert_eq!(parsed.content, None);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_1");
        assert_eq!(parsed.tool_calls[0].function.name, "create_file");
        assert_eq!(parsed.usage.expect("usage should parse").total_tokens, 15);
    }

    #[test]
    fn parses_single_bar_dsml_tool_call_from_content() {
        let content = r#"我会修改文件。
<｜DSML｜tool_calls><｜DSML｜invoke name="write_file"><｜DSML｜parameter name="path" string="true">\helloworld.txt</｜DSML｜parameter><｜DSML｜parameter name="content" string="true">123456</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"#;

        let calls = parse_dsml_tool_calls(content);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        assert_eq!(calls[0].function.arguments, r#"{"content":"123456","path":"helloworld.txt"}"#);
    }

    #[test]
    fn parses_double_bar_dsml_tool_call_from_content() {
        // 真实 DeepSeek 输出使用双竖线，旧解析器（硬编码单竖线）在此会漏掉，导致工具调用泄漏成正文。
        let content = "好的。\n<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"edit_file\"> <｜｜DSML｜｜parameter name=\"path\" string=\"true\">helloworld.txt</｜｜DSML｜｜parameter> <｜｜DSML｜｜parameter name=\"oldText\" string=\"true\">helloworld</｜｜DSML｜｜parameter> <｜｜DSML｜｜parameter name=\"newText\" string=\"true\">123456</｜｜DSML｜｜parameter> </｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>";

        let calls = parse_dsml_tool_calls(content);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "edit_file");
        let parsed: serde_json::Value =
            serde_json::from_str(&calls[0].function.arguments).expect("arguments should be json");
        assert_eq!(parsed["path"], "helloworld.txt");
        assert_eq!(parsed["oldText"], "helloworld");
        assert_eq!(parsed["newText"], "123456");
    }

    #[test]
    fn parses_user_balance_response() {
        let raw = serde_json::json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "CNY",
                "total_balance": "110.00",
                "granted_balance": "10.00",
                "topped_up_balance": "100.00"
            }]
        });
        let balance = parse_user_balance(&raw).expect("should parse");
        assert!(balance.is_available);
        assert_eq!(balance.balance_infos.len(), 1);
        assert_eq!(balance.balance_infos[0].currency, "CNY");
        assert_eq!(balance.balance_infos[0].total_balance, "110.00");
        assert_eq!(balance.balance_infos[0].topped_up_balance, "100.00");
    }

    #[test]
    fn parses_user_balance_with_missing_fields() {
        let balance = parse_user_balance(&serde_json::json!({})).expect("should parse empty");
        assert!(!balance.is_available);
        assert!(balance.balance_infos.is_empty());
    }

    #[test]
    fn classifies_retryable_errors() {
        assert!(DeepSeekError::ServerError.is_retryable());
        assert!(DeepSeekError::RateLimited.is_retryable());
        assert!(!DeepSeekError::Unauthorized.is_retryable());
        assert!(!DeepSeekError::InsufficientBalance.is_retryable());
        assert!(!DeepSeekError::ContextLengthExceeded.is_retryable());
        assert!(!DeepSeekError::BadRequest("x".to_string()).is_retryable());
    }

    #[test]
    fn strips_leaked_dsml_markup_keeping_narration() {
        // 模拟撞上限收尾时，模型把叙述 + DSML 调用一起吐进正文的情况。
        let content = "让我直接重写整个文件，移除所有 emoji 字符：\n\n<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"read_file\"> <｜｜DSML｜｜parameter name=\"path\" string=\"true\">src/CalculatorApp/calculator.py</｜｜DSML｜｜parameter> </｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>";

        let cleaned = strip_dsml_markup(content);

        assert_eq!(cleaned, "让我直接重写整个文件，移除所有 emoji 字符：");
        assert!(!cleaned.contains("DSML"));
    }

    #[test]
    fn strip_dsml_markup_returns_plain_text_untouched() {
        let content = "这是一段普通回复，没有任何工具标记。";
        assert_eq!(strip_dsml_markup(content), content);
    }

    #[test]
    fn parses_xmlish_toolcall_variant_from_content() {
        // 真实泄漏样本（0.0.17 dev 实测）：模型用 <ToolCall name="..."> XML 风格输出工具调用。
        let content = r#"好的，我来给 README.md 文件中增加一行 helloworld。

<ToolCall name="search_file"> <parameter name="target_directory" string="true">/</parameter> <parameter name="pattern" string="true">README.md</parameter> <parameter name="recursive" string="false">false</parameter> </ToolCall>"#;

        let calls = parse_dsml_tool_calls(content);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search_file");
        let parsed: serde_json::Value =
            serde_json::from_str(&calls[0].function.arguments).expect("arguments should be json");
        assert_eq!(parsed["pattern"], "README.md");
        assert_eq!(parsed["recursive"], "false");
    }

    #[test]
    fn tool_markup_index_detects_variants() {
        assert!(tool_markup_index("纯叙述，没有标记").is_none());
        assert!(tool_markup_index("先看文件 <DSMLinvoke").is_some());
        assert!(tool_markup_index("好的 <ToolCall name=").is_some());
        // 单/双竖线 DSML 变体
        assert!(tool_markup_index("好的 <\u{FF5C}DSML\u{FF5C}invoke").is_some());
        // 命中位置应在叙述之后
        let idx = tool_markup_index("叙述 <ToolCall x").unwrap();
        assert_eq!(&"叙述 <ToolCall x"[..idx], "叙述 ");
    }

    #[test]
    fn strips_leaked_toolcall_markup_keeping_narration() {
        let content = "我先找到文件：\n\n<ToolCall name=\"search_file\"> <parameter name=\"pattern\" string=\"true\">README.md</parameter> </ToolCall>";
        let cleaned = strip_dsml_markup(content);
        assert_eq!(cleaned, "我先找到文件：");
        assert!(!cleaned.contains("ToolCall"));
    }

    #[test]
    fn parses_multiple_dsml_tool_calls_from_content() {
        let content = "<｜｜DSML｜｜invoke name=\"read_file\"><｜｜DSML｜｜parameter name=\"path\" string=\"true\">a.txt</｜｜DSML｜｜parameter></｜｜DSML｜｜invoke><｜｜DSML｜｜invoke name=\"delete_file\"><｜｜DSML｜｜parameter name=\"path\" string=\"true\">b.txt</｜｜DSML｜｜parameter></｜｜DSML｜｜invoke>";

        let calls = parse_dsml_tool_calls(content);

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[1].function.name, "delete_file");
    }
}

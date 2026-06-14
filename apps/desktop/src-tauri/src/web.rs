//! 联网工具：web_fetch / web_search 及 HTML 转正文、DuckDuckGo 结果解析。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯函数搬移，无行为变更。这些工具只接收
//! arguments 字符串、不触碰 AppState，便于独立维护。

/// 把 HTML 粗略转为可读纯文本：去掉 script/style 整块，剥离标签，解码实体，压缩空白。
/// UTF-8 安全：按字符处理，不按字节，避免多字节中文被破坏。
pub(crate) fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    // 移除 script / style 整块（每轮重算 lower，保证偏移与 s 同步）
    for tag in ["script", "style"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        loop {
            let lower = s.to_lowercase();
            let Some(start) = lower.find(&open) else { break };
            match lower[start..].find(&close) {
                Some(rel) => {
                    let end = start + rel + close.len();
                    s.replace_range(start..end, " ");
                }
                None => {
                    s.replace_range(start.., " ");
                    break;
                }
            }
        }
    }
    // 按字符剥离标签
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// web_fetch 工具：抓取一个 URL 并提取可读正文（截断防膨胀）。仅允许 http/https。
pub(crate) async fn execute_web_fetch(arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let url = parsed.get("url").and_then(|v| v.as_str()).ok_or("web_fetch 缺少 url")?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http/https URL".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("NovaCode/1.0 (+https://github.com/aki66938/NovaCode)")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| format!("抓取失败: {e}"))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
    let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
        html_to_text(&body)
    } else {
        body
    };
    let capped: String = text.chars().take(40_000).collect();
    Ok(serde_json::json!({
        "url": url,
        "status": status.as_u16(),
        "text": capped
    }))
}

/// web_search 工具：通过 DuckDuckGo HTML 端点搜索，返回标题 / 链接 / 摘要列表（无需 API Key）。
pub(crate) async fn execute_web_search(arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let query = parsed.get("query").and_then(|v| v.as_str()).ok_or("web_search 缺少 query")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; NovaCode/1.0)")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("搜索失败: {e}"))?;
    let html = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
    let results = parse_ddg_results(&html, 8);
    if results.is_empty() {
        return Ok(serde_json::json!({
            "query": query,
            "results": [],
            "note": "未解析到结果（搜索页结构可能变化），可改用 web_fetch 直接抓取已知 URL。"
        }));
    }
    Ok(serde_json::json!({ "query": query, "results": results }))
}

/// 从 DuckDuckGo HTML 结果页解析前 limit 条 { title, url, snippet }。
fn parse_ddg_results(html: &str, limit: usize) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    // 结果链接锚点：class="result__a" href="..."；摘要：class="result__snippet"
    for chunk in html.split("result__a").skip(1) {
        if results.len() >= limit {
            break;
        }
        let Some(href_pos) = chunk.find("href=\"") else { continue };
        let after = &chunk[href_pos + 6..];
        let Some(end) = after.find('"') else { continue };
        let raw_url = &after[..end];
        let url = decode_ddg_url(raw_url);
        // 标题：href 标签 > 之后到 </a>
        let title = after
            .find('>')
            .and_then(|gt| after[gt + 1..].find("</a>").map(|e| &after[gt + 1..gt + 1 + e]))
            .map(|t| html_to_text(t))
            .unwrap_or_default();
        let snippet = chunk
            .find("result__snippet")
            .and_then(|p| chunk[p..].find('>').map(|gt| &chunk[p + gt + 1..]))
            .and_then(|s| s.find("</a>").map(|e| &s[..e]))
            .map(|s| html_to_text(s))
            .unwrap_or_default();
        if url.starts_with("http") && !title.is_empty() {
            results.push(serde_json::json!({
                "title": title,
                "url": url,
                "snippet": snippet.chars().take(300).collect::<String>()
            }));
        }
    }
    results
}

/// DuckDuckGo 跳转链接 `//duckduckgo.com/l/?uddg=<编码真实URL>` 解码为真实 URL。
fn decode_ddg_url(raw: &str) -> String {
    let target = if let Some(pos) = raw.find("uddg=") {
        let enc = &raw[pos + 5..];
        let enc = enc.split('&').next().unwrap_or(enc);
        percent_decode(enc)
    } else {
        raw.to_string()
    };
    if target.starts_with("//") {
        format!("https:{target}")
    } else {
        target
    }
}

/// 最小 percent-decode（仅用于 DDG 跳转 URL 还原）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::{html_to_text, percent_decode};

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><style>.a{color:red}</style></head><body><h1>标题</h1><script>alert(1)</script><p>正文 &amp; 内容</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("标题"));
        assert!(text.contains("正文 & 内容"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn percent_decode_restores_url() {
        assert_eq!(percent_decode("https%3A%2F%2Fa.com%2Fx"), "https://a.com/x");
    }
}

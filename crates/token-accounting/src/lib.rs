use novacode_shared::RawUsage;
use serde::{Deserialize, Serialize};

/// 单次请求的价格快照，版本化保存以支持历史费用回放。
///
/// 价格单位为美元 / 百万 token；每次价格调整新增版本，不覆盖历史记录。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PricingSnapshot {
    /// 价格版本标识，写入每条 token 记录，用于账单对照。
    pub version: String,
    pub input_cache_hit_per_1m: f64,
    pub input_cache_miss_per_1m: f64,
    pub output_per_1m: f64,
}

/// 当前内置的 DeepSeek V4 Flash 价格快照（2026-06 版）。
///
/// 来源：https://api-docs.deepseek.com/quick_start/pricing
/// 价格单位：美元 / 百万 token。
pub fn deepseek_v4_flash_pricing() -> PricingSnapshot {
    PricingSnapshot {
        version: "deepseek-v4-flash-2026-06".to_string(),
        input_cache_hit_per_1m: 0.0028,
        input_cache_miss_per_1m: 0.14,
        output_per_1m: 0.28,
    }
}

/// 当前内置的 DeepSeek V4 Pro 价格快照（2026-06 版）。
///
/// 输入为空，输出 V4 Pro 的价格快照；本方法不联网更新价格，价格更新需新增版本。
pub fn deepseek_v4_pro_pricing() -> PricingSnapshot {
    PricingSnapshot {
        version: "deepseek-v4-pro-2026-06".to_string(),
        input_cache_hit_per_1m: 0.003625,
        input_cache_miss_per_1m: 0.435,
        output_per_1m: 0.87,
    }
}

/// 根据 DeepSeek 模型 ID 选择对应价格快照。
///
/// 输入模型 ID，输出当前内置价格快照；废弃兼容别名按官方说明映射到 V4 Flash。
pub fn deepseek_pricing_for_model(model: &str) -> PricingSnapshot {
    match model {
        "deepseek-v4-pro" => deepseek_v4_pro_pricing(),
        "deepseek-v4-flash" | "deepseek-chat" | "deepseek-reasoner" => {
            deepseek_v4_flash_pricing()
        }
        _ => deepseek_v4_flash_pricing(),
    }
}

/// 单次请求的标准化 token 用量，用于 UI 展示和账单对照。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub prompt_cache_hit_tokens: u64,
    pub prompt_cache_miss_tokens: u64,
    pub completion_tokens: u64,
}

/// 单次请求经过费用计算后的完整摘要，由前端直接展示。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSummary {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    /// 估算费用，单位美元。
    pub estimated_cost_usd: f64,
    /// usage 来源：deepseek_usage | missing。
    pub usage_source: String,
    pub pricing_version: String,
}

/// 根据 DeepSeek token 用量和价格快照估算本次请求费用。
///
/// 输入标准化 usage 与价格版本，输出估算费用（美元）；
/// 本方法不读取官方账单，也不修改历史价格版本。
pub fn estimate_cost(usage: &TokenUsage, pricing: &PricingSnapshot) -> f64 {
    let hit = usage.prompt_cache_hit_tokens as f64 / 1_000_000.0 * pricing.input_cache_hit_per_1m;
    let miss =
        usage.prompt_cache_miss_tokens as f64 / 1_000_000.0 * pricing.input_cache_miss_per_1m;
    let out = usage.completion_tokens as f64 / 1_000_000.0 * pricing.output_per_1m;
    hit + miss + out
}

/// 将 DeepSeek 原始 usage 转换为带费用的摘要。
///
/// 输入服务端原始 usage 和当前价格快照，输出前端可直接展示的 CostSummary；
/// 本方法不写入数据库，调用方负责持久化。
pub fn compute_cost_summary(raw: &RawUsage, pricing: &PricingSnapshot) -> CostSummary {
    let usage = TokenUsage {
        prompt_cache_hit_tokens: raw.prompt_cache_hit_tokens,
        prompt_cache_miss_tokens: raw.prompt_cache_miss_tokens,
        completion_tokens: raw.completion_tokens,
    };
    let cost = estimate_cost(&usage, pricing);

    let usage_source = if raw.raw_json.is_empty() {
        "missing".to_string()
    } else {
        "deepseek_usage".to_string()
    };

    CostSummary {
        prompt_tokens: raw.prompt_tokens,
        completion_tokens: raw.completion_tokens,
        total_tokens: raw.total_tokens,
        cache_hit_tokens: raw.prompt_cache_hit_tokens,
        cache_miss_tokens: raw.prompt_cache_miss_tokens,
        reasoning_tokens: raw.reasoning_tokens,
        estimated_cost_usd: cost,
        usage_source,
        pricing_version: pricing.version.clone(),
    }
}

use novacode_token_accounting::{
    deepseek_pricing_for_model, deepseek_v4_flash_pricing, deepseek_v4_pro_pricing,
};

#[test]
fn returns_current_deepseek_v4_flash_pricing_by_default() {
    let pricing = deepseek_pricing_for_model("deepseek-v4-flash");

    assert_eq!(pricing.version, "deepseek-v4-flash-2026-06");
    assert_eq!(pricing.input_cache_hit_per_1m, 0.0028);
    assert_eq!(pricing.input_cache_miss_per_1m, 0.14);
    assert_eq!(pricing.output_per_1m, 0.28);
}

#[test]
fn returns_current_deepseek_v4_pro_pricing() {
    let pricing = deepseek_pricing_for_model("deepseek-v4-pro");

    assert_eq!(pricing, deepseek_v4_pro_pricing());
    assert_eq!(pricing.version, "deepseek-v4-pro-2026-06");
    assert_eq!(pricing.input_cache_hit_per_1m, 0.003625);
    assert_eq!(pricing.input_cache_miss_per_1m, 0.435);
    assert_eq!(pricing.output_per_1m, 0.87);
}

#[test]
fn deprecated_aliases_fall_back_to_flash_pricing_for_compatibility() {
    assert_eq!(deepseek_pricing_for_model("deepseek-chat"), deepseek_v4_flash_pricing());
    assert_eq!(deepseek_pricing_for_model("deepseek-reasoner"), deepseek_v4_flash_pricing());
}

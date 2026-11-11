use std::path::Path;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::api::conversation::context_kernel;

/// 只读：返回当前会话的上下文内核状态 JSON（块、用量、阈值、content store 统计）。
///
/// 仅供 UI 展示；`conversation_id` 为空时返回空串。子代理视图不暴露给 UI。
#[napi]
pub fn get_context_kernel_status(conversation_id: String) -> Result<String> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Ok(String::new());
    }
    let storage = crate::storage::initialize_app_storage()?;
    let snapshot = context_kernel::status_snapshot(
        Path::new(&storage.database_path),
        &conversation_id,
    )?;
    Ok(snapshot.to_string())
}

/// 只读：上下文内核自检。
///
/// 返回一组**纯函数**的实测输出（refs / segment / viable / boundaries / protected /
/// recommend / nudge-text / output-steering / report / content-store / ccr / absorb /
/// crush / image-compress / surface-config / parse-compress-input / cache-report）。
/// 没有副作用、不读数据库；`scripts/verify-kernel-behavior.mjs` 用它把移植结果与上游
/// `acp-kernel` 的实测行为逐条比对，作为回归基线。
#[napi]
pub fn context_kernel_self_test() -> Result<String> {
    use crate::api::conversation::context_kernel as k;
    use serde_json::json;

    let mut out = serde_json::Map::new();
    let mut put = |key: &str, value: serde_json::Value| {
        out.insert(key.to_string(), value);
    };

    // ---- refs ----
    put(
        "index_to_ref",
        json!([
            k::refs::index_to_ref(0),
            k::refs::index_to_ref(1),
            k::refs::index_to_ref(41),
            k::refs::index_to_ref(999_999),
            k::refs::index_to_ref(10_000_000),
        ]),
    );
    put(
        "ref_to_index",
        json!([
            k::refs::ref_to_index("m00000"),
            k::refs::ref_to_index("m00001"),
            k::refs::ref_to_index("m00041"),
            k::refs::ref_to_index("garbage"),
            k::refs::ref_to_index("m12"),
        ]),
    );
    put(
        "base_id_of",
        json!([
            k::refs::base_id_of("a#b"),
            k::refs::base_id_of("a#r0"),
            k::refs::base_id_of("plain"),
            k::refs::base_id_of("x_y#z"),
        ]),
    );

    // ---- segments ----
    let items: Vec<k::segments::SegmentItem> = vec![
        ("m1", false, true),
        ("m2", false, false),
        ("m3", false, false),
        ("m4", false, true),
        ("m5", true, false),
    ]
    .into_iter()
    .map(|(r, gap, user)| k::segments::SegmentItem {
        r#ref: r.to_string(),
        gap_before: gap,
        is_user: user,
    })
    .collect();
    put(
        "segment_groups",
        json!(k::segments::segment_groups(&items, |item| item.gap_before, |item| item.is_user)
            .iter()
            .map(|group| group.iter().map(|item| item.r#ref.clone()).collect::<Vec<_>>())
            .collect::<Vec<_>>()),
    );
    put(
        "viable_ranges",
        json!(k::segments::viable_ranges(&[199i64, 200, 800])
            .into_iter()
            .collect::<Vec<_>>()),
    );
    put(
        "parse_boundary",
        json!(["m00012", "b3", "BLOCKED", "m12", "x"]
            .iter()
            .map(|raw| {
                k::segments::parse_boundary(raw).map(|parsed| {
                    json!({
                        "kind": match parsed.kind {
                            k::segments::BoundaryKind::Message => "message",
                            k::segments::BoundaryKind::Block => "block",
                        },
                        "numericId": parsed.numeric_id,
                        "raw": parsed.raw,
                    })
                })
            })
            .collect::<Vec<_>>()),
    );
    put(
        "adjust_tool_pairs",
        json!({
            "none": k::segments::adjust_boundaries_for_tool_pairs(&Default::default(), 0, 3),
        }),
    );

    // ---- steering ----
    put(
        "render_steering_block",
        json!([0, 1, 2, 3, 4].iter()
            .map(|level| k::texts::render_steering_block(*level, k::texts::DEFAULT_STEERING_SENTINEL))
            .collect::<Vec<_>>()),
    );
    let structural_tool = vec![k::steering::StructuralMessage {
        role: "user".to_string(),
        blocks: vec![k::steering::StructuralBlock {
            kind: k::steering::StructuralBlockKind::ToolResult,
            is_error: false,
        }],
    }];
    let structural_err = vec![k::steering::StructuralMessage {
        role: "user".to_string(),
        blocks: vec![k::steering::StructuralBlock {
            kind: k::steering::StructuralBlockKind::ToolResult,
            is_error: true,
        }],
    }];
    let structural_text = vec![k::steering::StructuralMessage {
        role: "user".to_string(),
        blocks: vec![k::steering::StructuralBlock {
            kind: k::steering::StructuralBlockKind::Text,
            is_error: false,
        }],
    }];
    put(
        "classify_turn",
        json!([
            k::steering::classify_turn(&[]).as_str(),
            k::steering::classify_turn(&structural_text).as_str(),
            k::steering::classify_turn(&structural_tool).as_str(),
            k::steering::classify_turn(&structural_err).as_str(),
        ]),
    );
    let off = k::steering::OutputSteeringConfig::default();
    let on = k::steering::OutputSteeringConfig {
        enabled: true,
        verbosity_level: 2,
        effort_routing: true,
    };
    put(
        "decide_output_steering",
        json!([
            {
                "turnKind": k::steering::decide_output_steering(&structural_tool, &off).turn_kind.as_str(),
                "verbosityLevel": k::steering::decide_output_steering(&structural_tool, &off).verbosity_level,
                "lowerEffort": k::steering::decide_output_steering(&structural_tool, &off).lower_effort,
            },
            {
                "turnKind": k::steering::decide_output_steering(&structural_tool, &on).turn_kind.as_str(),
                "verbosityLevel": k::steering::decide_output_steering(&structural_tool, &on).verbosity_level,
                "lowerEffort": k::steering::decide_output_steering(&structural_tool, &on).lower_effort,
            },
            {
                "turnKind": k::steering::decide_output_steering(&structural_text, &on).turn_kind.as_str(),
                "verbosityLevel": k::steering::decide_output_steering(&structural_text, &on).verbosity_level,
                "lowerEffort": k::steering::decide_output_steering(&structural_text, &on).lower_effort,
            },
        ]),
    );
    put(
        "clamp_effort",
        json!([
            k::steering::clamp_effort_to_floor(Some("high"), Some("low")),
            k::steering::clamp_effort_to_floor(Some("minimal"), Some("low")),
            k::steering::clamp_effort_to_floor(Some("medium"), Some("high")),
            k::steering::clamp_effort_to_floor(None, Some("low")),
            k::steering::clamp_effort_to_floor(Some("bogus"), Some("low")),
            k::steering::clamp_effort_to_floor(Some("low"), None),
        ]),
    );
    let block = k::texts::render_steering_block(2, k::texts::DEFAULT_STEERING_SENTINEL).unwrap();
    let fresh = k::steering::apply_steering_to_prompt("SYS", &block, k::texts::DEFAULT_STEERING_SENTINEL);
    let idem = k::steering::apply_steering_to_prompt(
        &fresh.updated,
        &block,
        k::texts::DEFAULT_STEERING_SENTINEL,
    );
    put(
        "apply_steering",
        json!({
            "fresh": {"updated": fresh.updated, "changed": fresh.changed},
            "idem": {"updated": idem.updated, "changed": idem.changed},
        }),
    );
    put(
        "resolve_verbosity",
        json!([
            {"level": k::steering::resolve_verbosity_level(None).0, "warning": k::steering::resolve_verbosity_level(None).1},
            {"level": k::steering::resolve_verbosity_level(Some(&json!(2))).0},
            {"level": k::steering::resolve_verbosity_level(Some(&json!(9))).0, "warning": k::steering::resolve_verbosity_level(Some(&json!(9))).1},
            {"level": k::steering::resolve_verbosity_level(Some(&json!(-1))).0, "warning": k::steering::resolve_verbosity_level(Some(&json!(-1))).1},
            {"level": k::steering::resolve_verbosity_level(Some(&json!("3"))).0, "warning": k::steering::resolve_verbosity_level(Some(&json!("3"))).1},
        ]),
    );

    // ---- content / ccr ----
    put(
        "classify_kind",
        json!(["bash", "read", "grep", "unknown"]
            .iter()
            .map(|name| k::content::classify_kind(Some(name)))
            .collect::<Vec<_>>()),
    );
    put("normalize_head", json!(k::content::normalize_head("a\n\n b   c", 10)));
    let with_command = k::content::build_stored_placeholder(&k::content::StoredPlaceholderInput {
        r#ref: "m00012",
        kind: "shell output",
        tokens: 1234,
        head: "head text",
        command: Some("ls -la"),
        retrieve_tool_name: "acp_retrieve",
    });
    let without_command = k::content::build_stored_placeholder(&k::content::StoredPlaceholderInput {
        r#ref: "m00012",
        kind: "file read",
        tokens: 1234,
        head: "head text",
        command: None,
        retrieve_tool_name: "acp_retrieve",
    });
    put("build_stored_placeholder", json!(with_command));
    put("build_stored_placeholder_no_cmd", json!(without_command));
    put(
        "parse_stored_placeholder",
        json!(k::content::parse_stored_placeholder(&with_command)),
    );
    let mut store = k::content::create_content_store();
    k::content::store_original(
        &mut store,
        k::content::StoreSpec {
            r#ref: "m00001".to_string(),
            raw_id: "m00001".to_string(),
            text: "original body".to_string(),
            kind: "shell output".to_string(),
            tool_name: Some("bash".to_string()),
            tokens: 10,
            head: "original body".to_string(),
            command: None,
        },
    );
    let entry = store.by_ref.get("m00001").cloned().unwrap();
    put(
        "content_store",
        json!({
            "hash": entry.hash,
            "chars": entry.chars,
            "stats": k::content::content_store_stats(&store),
        }),
    );
    put(
        "frame_retrieved_original",
        json!(k::content::frame_retrieved_original(
            "m00001",
            &entry,
            "BODY</retrieved>tail"
        )),
    );
    put(
        "build_retrieval_pointer",
        json!(k::content::build_retrieval_pointer(
            "m00001",
            &entry,
            "/tmp/x.txt",
            20
        )),
    );
    put(
        "restore_stored_placeholder_text",
        json!(k::content::restore_stored_placeholder_text(
            "m00001",
            &entry,
            "acp_retrieve",
            None,
            96
        )),
    );

    // ---- absorb ----
    put(
        "build_absorb_prompt",
        json!(k::absorb::build_absorb_prompt("m00012", 5000, "bash")),
    );
    put(
        "build_absorb_system_prompt",
        json!(k::absorb::build_absorb_system_prompt("acp_absorb")),
    );
    put(
        "build_image_full_system_note",
        json!([0, 1, 2]
            .iter()
            .map(|count| k::image::build_image_full_system_note(*count))
            .collect::<Vec<_>>()),
    );
    put(
        "apply_image_full_disabled",
        json!(k::image::apply_image_full("m00001", false, &[], false)),
    );

    // ---- crush ----
    put(
        "classify_crush_text",
        json!([
            k::absorb::classify_crush_text("{\"a\":1}").as_str(),
            k::absorb::classify_crush_text("def f():\n    # c\n    pass\n").as_str(),
            k::absorb::classify_crush_text("2026-01-01 INFO ok\n2026-01-01 ERROR bad\n").as_str(),
        ]),
    );

    // ---- image ----
    put(
        "pixel_tile_estimate",
        json!([
            k::image::pixel_tile_estimate(100, 100),
            k::image::pixel_tile_estimate(800, 600),
            k::image::pixel_tile_estimate(2000, 1000),
            k::image::pixel_tile_estimate(4000, 4000),
        ]),
    );
    put(
        "estimate_image_tokens",
        json!([
            k::image::estimate_image_tokens(&k::image::ImageMeta {
                media_type: "image/png".to_string(),
                base64_length: 4000,
                ..Default::default()
            }),
            k::image::estimate_image_tokens(&k::image::ImageMeta {
                media_type: "image/png".to_string(),
                base64_length: 4000,
                width: Some(1000),
                height: Some(800),
                billing_pixels: true,
                ..Default::default()
            }),
        ]),
    );
    let meta_screenshot = k::image::ImageMeta {
        media_type: "image/png".to_string(),
        base64_length: 400_000,
        width: Some(1920),
        height: Some(1080),
        ..Default::default()
    };
    let route = |meta: &k::image::ImageMeta, enabled: bool, min_tokens: i64| {
        let decision = k::image::decide_image_route(meta, enabled, min_tokens, 1280, 80, "webp", 1.4, 2.6, 720);
        json!({
            "action": decision.action,
            "reason": decision.reason.as_str(),
            "estimatedTokens": decision.estimated_tokens,
            "recipe": decision.recipe,
        })
    };
    put("decide_image_route_off", route(&meta_screenshot, false, 512));
    put("decide_image_route_on", route(&meta_screenshot, true, 512));
    put(
        "decide_image_route_not_screenshot",
        route(
            &k::image::ImageMeta {
                media_type: "image/png".to_string(),
                base64_length: 400_000,
                width: Some(800),
                height: Some(800),
                ..Default::default()
            },
            true,
            512,
        ),
    );
    put("decide_image_route_not_image", route(
        &k::image::ImageMeta {
            media_type: "text/plain".to_string(),
            base64_length: 400_000,
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        },
        true,
        512,
    ));
    put(
        "parse_image_dimensions_png",
        json!(k::image::parse_image_dimensions(&[
            0x89, 0x50, 0x4e, 0x47, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 2, 0, 0, 0, 1,
            0
        ])),
    );

    // ---- protection: formatRanges ----
    put(
        "format_ranges",
        json!(k::protection::format_ranges(
            &[
                k::protection::CompressibleRange {
                    start_ref: "m00001".to_string(),
                    end_ref: "m00010".to_string(),
                    start_index: 0,
                    end_index: 9,
                    count: 10,
                    tokens: 5000,
                    chars: 5000,
                    user_msgs: 2,
                    tool_tokens: 2000,
                    text_tokens: 3000,
                    dangerous: false,
                },
                k::protection::CompressibleRange {
                    start_ref: "m00020".to_string(),
                    end_ref: "m00030".to_string(),
                    start_index: 19,
                    end_index: 29,
                    count: 11,
                    tokens: 900,
                    chars: 900,
                    user_msgs: 0,
                    tool_tokens: 720,
                    text_tokens: 180,
                    dangerous: true,
                },
            ],
            &[k::protection::ProtectedRange {
                start_ref: "m00011".to_string(),
                end_ref: "m00012".to_string(),
                start_index: 10,
                end_index: 11,
                count: 2,
                tokens: 300,
                tools: vec!["skill".to_string(), "task".to_string()],
            }],
        )),
    );
    put(
        "format_ranges_empty",
        json!(k::protection::format_ranges(&[], &[])),
    );
    put(
        "format_breakdown_empty",
        json!(k::protection::format_breakdown(0, 0, 0, 0, 0, 0)),
    );
    put(
        "format_block_map_empty",
        json!(k::protection::format_block_map(&[], 8)),
    );

    // ---- porting: surface config / parse input / prompts ----
    let mut overrides: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    overrides.insert("tools".to_string(), None);
    overrides.insert("acpTags".to_string(), Some("A2".to_string()));
    put(
        "apply_section_overrides",
        json!(k::porting::apply_section_overrides(
            &[
                ("acpTags".to_string(), "A".to_string()),
                ("tools".to_string(), "B".to_string()),
                ("summariesInContext".to_string(), "C".to_string()),
            ],
            &overrides,
        )),
    );
    let mut descriptions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    descriptions.insert("a".to_string(), "X".to_string());
    descriptions.insert("c".to_string(), "Y".to_string());
    put(
        "clone_with_descriptions",
        json!(k::porting::clone_with_descriptions(
            &json!({
                "type": "object",
                "properties": {
                    "a": {"description": "d1"},
                    "b": {"type": "array", "items": {"properties": {"c": {"description": "d2"}}}}
                }
            }),
            &descriptions,
        )),
    );
    put(
        "parse_compress_args",
        json!([
            k::porting::parse_compress_args(&json!(
                "{\"content\":[{\"startRef\":\"m00001\",\"endRef\":\"m00010\",\"summary\":\"s\"}]}"
            )),
            k::porting::parse_compress_args(
                &serde_json::from_str("{\"content\":[{\"start_ref\":\"m00001\"}]}").unwrap()
            ),
            k::porting::parse_compress_args(&json!("not json")),
        ]),
    );
    put(
        "derive_topic_from_summary",
        json!(k::porting::derive_topic_from_summary("# My Topic\nbody")),
    );
    let prompts = k::porting::resolve_prompts(&Default::default(), false, true).unwrap();
    put(
        "resolve_prompts_language",
        json!({
            "philosophyEndsWithRule": prompts.compress_philosophy.ends_with(k::texts::LANGUAGE_PRESERVATION_RULE),
            "philosophyLength": prompts.compress_philosophy.chars().count(),
        }),
    );
    put(
        "resolve_prompts_requires_ack",
        json!(k::porting::resolve_prompts(
            &[("compressPhilosophy".to_string(), Some("X".to_string()))]
                .into_iter()
                .collect(),
            false,
            false,
        )
        .is_err()),
    );

    // ---- analytics ----
    let samples = vec![
        k::analytics::CacheSample {
            at: 1000,
            input: 10_000,
            cached: 0,
            output: 100,
        },
        k::analytics::CacheSample {
            at: 2000,
            input: 20_000,
            cached: 10_000,
            output: 100,
        },
        k::analytics::CacheSample {
            at: 3000,
            input: 25_000,
            cached: 18_000,
            output: 120,
        },
    ];
    let fold_events = vec![k::analytics::FoldEvent {
        at: 1500,
        tokens_compressed: 5000,
        summary_tokens: 200,
        first_fold_start_tokens: Some(9000),
        view_before: Some(20_000),
        view_after: Some(15_000),
    }];
    let report = k::analytics::build_cache_report(&samples, &fold_events, 512, 0);
    put("cache_report_totals", json!(report.totals));
    put("cache_report_economics", json!(report.economics));
    put(
        "decompose_sample",
        json!(k::analytics::decompose_sample(
            Some(&samples[1]),
            &samples[2],
            &[]
        )),
    );

    // ---- 工具路由 ----
    //
    // 回归基线：`server_id` 自带连字符（`context-kernel`），它必须在
    // `BUILTIN_SERVER_IDS` 里登记，否则 `context-kernel-acp_status` 会被拆成
    // server="context"、tool="kernel-acp_status"，落到外部服务器解析分支并以
    // "MCP tool is no longer available" 失败（实测踩过一次，10 个工具全废）。
    let kernel_tools = [
        "context-kernel-compress",
        "context-kernel-absorb",
        "context-kernel-decompress",
        "context-kernel-retrieve",
        "context-kernel-search_context",
        "context-kernel-acp_status",
        "context-kernel-acp_cache",
        "context-kernel-acp_rule",
        "context-kernel-recap",
        "context-kernel-handoff",
    ];
    let builtin_tool_names: Vec<String> = crate::mcp::builtin::get_builtin_tools()
        .iter()
        .map(crate::mcp::tools::McpTool::full_name)
        .collect();
    put(
        "tool_routing",
        json!({
            "split": kernel_tools
                .iter()
                .map(|name| crate::mcp::tools::split_tool_full_name(name))
                .collect::<Vec<_>>(),
            "in_builtin_list": kernel_tools
                .iter()
                .map(|name| builtin_tool_names.iter().any(|entry| entry == name))
                .collect::<Vec<_>>(),
            "recognised_as_kernel": kernel_tools
                .iter()
                .map(|name| k::is_context_kernel_tool(name))
                .collect::<Vec<_>>(),
        }),
    );

    // ---- 建议区间选取（真机回归：单用户轮 + 工具循环曾返回空 suggestedRanges）----
    put("range_selection", k::self_test_range_selection());

    Ok(serde_json::Value::Object(out).to_string())
}


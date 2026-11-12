//! 上游 `image-compress.ts`（issue #353）：截图类工具结果在**到达时**下采样一次再上
//! 线；非截图原文原样通过；有损一步由 `image_full` 复原通道兜底。
//!
//! 内核只管**纯函数**部分：路由决策 + 配方、计费感知的 token 估算、`image_full` 契约
//! （参数解析、粘性复原状态机、结果串、系统提示词片段）。真正的编码（sharp 等）与
//! sidecar 文件 I/O 属于宿主。
//!
//! 确定性是承重的：下采样后的字节会成为常驻的线上内容，配方不确定就会在会话中途
//! 击穿 provider 前缀缓存。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use super::texts;

/// 维度不可解析时的最坏高细节计费。
pub const PIXEL_IMAGE_FALLBACK_TOKENS: i64 = texts::PIXEL_IMAGE_FALLBACK_TOKENS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

/// 只读容器头部的维数解析：PNG / GIF / BMP / WebP（VP8X、VP8L、VP8 有损）/ JPEG
/// （SOF 段扫描）。未知格式或头部被截断 → `None`。
pub fn parse_image_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    // PNG：8 字节签名 + IHDR
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 13, 10, 26, 10] {
        let width = be_u32(bytes, 16)?;
        let height = be_u32(bytes, 20)?;
        return Some(ImageDimensions { width, height });
    }
    // GIF：`GIF87a` / `GIF89a` + 逻辑屏幕描述符（小端 16 位）
    if bytes.len() >= 10 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        let width = le_u16(bytes, 6)? as u32;
        let height = le_u16(bytes, 8)? as u32;
        return Some(ImageDimensions { width, height });
    }
    // BMP：`BM` + DIB 头（偏移 18/22 为小端 32 位）
    if bytes.len() >= 26 && &bytes[..2] == b"BM" {
        let width = le_u32(bytes, 18)? as i32;
        let height = le_u32(bytes, 22)? as i32;
        return Some(ImageDimensions {
            width: width.unsigned_abs(),
            height: height.unsigned_abs(),
        });
    }
    // WebP：`RIFF`…`WEBP` + 块
    if bytes.len() >= 30 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        match &bytes[12..16] {
            b"VP8X" => {
                let width = 1 + (u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16));
                let height = 1 + (u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16));
                return Some(ImageDimensions { width, height });
            }
            b"VP8L" => {
                let bits = u32::from_le_bytes([
                    bytes[21], bytes[22], bytes[23], bytes[24],
                ]);
                let width = (bits & 0x3FFF) + 1;
                let height = ((bits >> 14) & 0x3FFF) + 1;
                return Some(ImageDimensions { width, height });
            }
            b"VP8 " => {
                let width = (u32::from(le_u16(bytes, 26)?) & 0x3FFF) as u32;
                let height = (u32::from(le_u16(bytes, 28)?) & 0x3FFF) as u32;
                return Some(ImageDimensions { width, height });
            }
            _ => {}
        }
    }
    // JPEG：SOI + 段扫描找 SOF0/1/2/3/5/6/7/9/10/11/13/14/15
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut offset = 2usize;
        while offset + 9 < bytes.len() {
            if bytes[offset] != 0xFF {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            if (0xC0..=0xCF).contains(&marker)
                && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
            {
                let height = u32::from(be_u16(bytes, offset + 5)?);
                let width = u32::from(be_u16(bytes, offset + 7)?);
                return Some(ImageDimensions { width, height });
            }
            let segment_length = u32::from(be_u16(bytes, offset + 2)?) as usize;
            if segment_length < 2 {
                return None;
            }
            offset += 2 + segment_length;
        }
    }
    None
}

fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

/// base64 便捷入口：只解码**头部切片** —— 容器头够用（PNG/GIF/BMP/WebP 64 字符内，
/// JPEG SOF 在 EXIF 膨胀下约 350K 字符内）。
pub fn parse_image_dimensions_from_base64(b64: &str) -> Option<ImageDimensions> {
    const DECODE_BUDGET: usize = 400_000;
    let budget = b64.len().min(DECODE_BUDGET);
    let mut bytes: Vec<u8> = Vec::with_capacity(budget / 4 * 3);
    let alphabet = |ch: u8| -> Option<u8> {
        Some(match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let raw = b64.as_bytes();
    let mut index = 0usize;
    while index + 4 <= budget {
        let chunk = &raw[index..index + 4];
        let Some(a) = alphabet(chunk[0]) else { break };
        let Some(b) = alphabet(chunk[1]) else { break };
        let c = alphabet(chunk[2]);
        let d = alphabet(chunk[3]);
        bytes.push((a << 2) | (b >> 4));
        match c {
            Some(c) => {
                bytes.push((b << 4) | (c >> 2));
                if let Some(d) = d {
                    bytes.push((c << 6) | d);
                }
            }
            None => break,
        }
        if d.is_none() {
            break;
        }
        index += 4;
    }
    parse_image_dimensions(&bytes)
}

/// OpenAI 高细节瓦片模型：短边至少缩放到 768（provider 会给小图放大后计费），长边
/// 上限 2048，然后 `85 + 170 × 512×512 瓦片数`。取值区间约 [765, 2805]。
pub fn pixel_tile_estimate(width: u32, height: u32) -> i64 {
    if width == 0 || height == 0 {
        return PIXEL_IMAGE_FALLBACK_TOKENS;
    }
    let (mut w, mut h) = (width as f64, height as f64);
    let short = w.min(h);
    if short < 768.0 {
        let scale = 768.0 / short;
        w *= scale;
        h *= scale;
    }
    let long = w.max(h);
    if long > 2048.0 {
        let scale = 2048.0 / long;
        w *= scale;
        h *= scale;
    }
    let tiles = (w / 512.0).ceil().max(1.0) * (h / 512.0).ceil().max(1.0);
    85 + 170 * tiles as i64
}

/// 一张图片附件在路由决策眼里的元数据。
#[derive(Clone, Debug, Default)]
pub struct ImageMeta {
    pub media_type: String,
    pub base64_length: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 上游计费模式：`bytes`（默认，保守）或 `pixels`。
    pub billing_pixels: bool,
    pub base64: Option<String>,
}

/// 计费感知的图片 token 估算。pixels 模式用真实维度跑瓦片模型（缺失时只解头部）；
/// bytes 模式按 `ceil(len/4)`。pixels 模式维度不可知时回落最坏值。
pub fn estimate_image_tokens(meta: &ImageMeta) -> i64 {
    if meta.billing_pixels {
        let dimensions = match (meta.width, meta.height) {
            (Some(width), Some(height)) => Some(ImageDimensions { width, height }),
            _ => meta
                .base64
                .as_deref()
                .and_then(parse_image_dimensions_from_base64),
        };
        return match dimensions {
            Some(dimensions) => pixel_tile_estimate(dimensions.width, dimensions.height),
            None => PIXEL_IMAGE_FALLBACK_TOKENS,
        };
    }
    ((meta.base64_length as i64) + 3) / 4
}

/// v1 启发式截图判定：长/短边比落在 [1.4, 2.6] **且**短边 ≥ 720 px。
/// 维度缺失 → false（保守：分类不了就绝不路由）。
pub fn is_screenshot_like(meta: &ImageMeta, aspect_min: f64, aspect_max: f64, min_short_side: u32) -> bool {
    let (Some(width), Some(height)) = (meta.width, meta.height) else {
        return false;
    };
    if width == 0 || height == 0 {
        return false;
    }
    let aspect = f64::from(width.max(height)) / f64::from(width.min(height));
    aspect >= aspect_min && aspect <= aspect_max && width.min(height) >= min_short_side
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageRouteReason {
    Disabled,
    NotAnImage,
    BelowMinTokens,
    NotScreenshotLike,
    Downsample,
}

impl ImageRouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageRouteReason::Disabled => "disabled",
            ImageRouteReason::NotAnImage => "not-an-image",
            ImageRouteReason::BelowMinTokens => "below-min-tokens",
            ImageRouteReason::NotScreenshotLike => "not-screenshot-like",
            ImageRouteReason::Downsample => "downsample",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownsampleRecipe {
    pub max_dimension: u32,
    pub quality: u8,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRoutingDecision {
    pub action: &'static str,
    pub reason: ImageRouteReason,
    pub estimated_tokens: i64,
    pub recipe: Option<DownsampleRecipe>,
}

/// 纯 + 确定性：同样的输入 + 配置永远给出同样的决策。宿主在到达时**恰好执行一次**
/// 返回的配方。
#[allow(clippy::too_many_arguments)]
pub fn decide_image_route(
    meta: &ImageMeta,
    enabled: bool,
    min_tokens: i64,
    max_dimension: u32,
    quality: u8,
    format: &str,
    aspect_min: f64,
    aspect_max: f64,
    min_short_side: u32,
) -> ImageRoutingDecision {
    let estimated = estimate_image_tokens(meta);
    if !enabled {
        return ImageRoutingDecision {
            action: "pass",
            reason: ImageRouteReason::Disabled,
            estimated_tokens: estimated,
            recipe: None,
        };
    }
    if !meta.media_type.trim().to_ascii_lowercase().starts_with("image/") {
        return ImageRoutingDecision {
            action: "pass",
            reason: ImageRouteReason::NotAnImage,
            estimated_tokens: estimated,
            recipe: None,
        };
    }
    if estimated < min_tokens {
        return ImageRoutingDecision {
            action: "pass",
            reason: ImageRouteReason::BelowMinTokens,
            estimated_tokens: estimated,
            recipe: None,
        };
    }
    if !is_screenshot_like(meta, aspect_min, aspect_max, min_short_side) {
        return ImageRoutingDecision {
            action: "pass",
            reason: ImageRouteReason::NotScreenshotLike,
            estimated_tokens: estimated,
            recipe: None,
        };
    }
    ImageRoutingDecision {
        action: "downsample",
        reason: ImageRouteReason::Downsample,
        estimated_tokens: estimated,
        recipe: Some(DownsampleRecipe {
            max_dimension,
            quality,
            format: format.to_string(),
        }),
    }
}

/// 字节稳定的系统提示词片段；`count` = 本会话已下采样的图片数。
pub fn build_image_full_system_note(count: usize) -> String {
    format!(
        "[Downscaled screenshots: {count} image(s) were reduced before entering context. If you cannot read details (text, colors, alignment) in a reduced image, call {} with that message's ref (\"mNNNNN\") to restore the original resolution.]",
        texts::IMAGE_FULL_TOOL_NAME
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedImageFull {
    pub r#ref: String,
    pub call_id: Option<String>,
}

/// 宽松参数解析（与 absorb 同形）：接受对象或 JSON 字符串载荷、`ref`/`messageId`/`of`
/// 别名，并 trim 空白。
pub fn parse_image_full_input(
    input: &serde_json::Value,
    call_id: Option<&str>,
) -> Option<ParsedImageFull> {
    super::absorb::parse_lenient(input, &["ref", "messageId", "message_id", "of", "id"], &[])
        .map(|(reference, _)| ParsedImageFull {
            r#ref: reference,
            call_id: call_id.map(ToString::to_string),
        })
}

/// 粘性复原状态机。
///
/// 成功会把该 ref 标记为「本会话已复原」（重复调用是幂等的，报告 already restored）。
/// 失败模式返回 `ok: false` 且结果串以 `[image_full FAILED:` 开头。
pub fn apply_image_full(
    reference: &str,
    enabled: bool,
    shrunk_refs: &[String],
    already_restored: bool,
) -> (bool, String) {
    if !enabled {
        return (
            false,
            format!(
                "{} image compression is disabled in this session — no image was ever downscaled]",
                texts::IMAGE_FULL_FAILURE_MARKER
            ),
        );
    }
    if !shrunk_refs.iter().any(|value| value == reference) {
        return (
            false,
            format!(
                "{} no downscaled image is recorded for {reference} in this session — nothing to restore]",
                texts::IMAGE_FULL_FAILURE_MARKER
            ),
        );
    }
    if already_restored {
        return (
            true,
            format!(
                "Full resolution already active for {reference} — the original image is being sent for the rest of this session."
            ),
        );
    }
    (
        true,
        format!(
            "Full resolution restored for {reference} — the original image replaces the downscaled copy for the rest of this session."
        ),
    )
}

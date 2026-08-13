use std::path::Path;

use napi::bindgen_prelude::*;
use rusqlite::params;
use serde_json::json;

use super::super::database;
use super::super::{ChatConversationRecord};
use super::chat_conversations;

mod csv;
mod html;
mod json;
mod markdown;

use self::csv::render_csv;
use self::html::render_html;
use self::json::render_json;
use self::markdown::render_markdown;

/// Supported export formats.
pub const FORMAT_MARKDOWN: &str = "markdown";
pub const FORMAT_HTML: &str = "html";
pub const FORMAT_JSON: &str = "json";
pub const FORMAT_CSV: &str = "csv";

/// 获取会话记录和全部消息，然后按照指定格式生成导出内容。
/// 所有 SQLite I/O 由调用方的 spawn_blocking 包裹，不会阻塞 Node.js 主线程。
pub fn export_conversation(
    database_path: &Path,
    conversation_id: &str,
    format: &str,
) -> Result<String> {
    let conversation = chat_conversations::get_chat_conversation(database_path, conversation_id)?
        .ok_or_else(|| {
        Error::new(
            Status::GenericFailure,
            format!("Conversation not found: {conversation_id}"),
        )
    })?;

    let messages = chat_conversations::list_chat_messages(database_path, conversation_id)?;

    let content = match format {
        FORMAT_MARKDOWN => render_markdown(&conversation, &messages),
        FORMAT_HTML => render_html(&conversation, &messages),
        FORMAT_JSON => render_json(&conversation, &messages)?,
        FORMAT_CSV => render_csv(&conversation, &messages)?,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("Unsupported export format: {format}"),
            ));
        }
    };

    Ok(content)
}

/// 返回指定格式的文件扩展名（不含点号）。
#[allow(dead_code)]
pub fn extension_for_format(format: &str) -> &'static str {
    match format {
        FORMAT_MARKDOWN => "md",
        FORMAT_HTML => "html",
        FORMAT_JSON => "json",
        FORMAT_CSV => "csv",
        _ => "txt",
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn display_title(conversation: &ChatConversationRecord) -> String {
    let title = if !conversation.summary.is_empty() {
        &conversation.summary
    } else if !conversation.title.is_empty() {
        &conversation.title
    } else {
        "Untitled"
    };
    title.to_string()
}

fn normalize_role(role: &str) -> String {
    let trimmed = role.trim().to_lowercase();
    match trimmed.as_str() {
        "user" | "human" => "user".to_string(),
        "assistant" | "ai" => "assistant".to_string(),
        "system" => "system".to_string(),
        "developer" => "developer".to_string(),
        "tool" | "function" => "tool".to_string(),
        _ => trimmed,
    }
}

fn role_label(role: &str) -> String {
    let upper = role[..1].to_uppercase() + &role[1..];
    upper
}

fn role_css_class(role: &str) -> &'static str {
    match role {
        "user" => "user",
        "assistant" => "assistant",
        "tool" => "tool",
        "system" => "system",
        "developer" => "developer",
        _ => "system",
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[allow(dead_code)]
fn count_messages(database_path: &Path, conversation_id: &str) -> Result<i64> {
    database::open_connection(database_path)
        .and_then(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1",
                params![conversation_id],
                |row| row.get(0),
            )
        })
        .map_err(|error| database::database_error(database_path, "count chat messages", error))
}

#[allow(dead_code)]
fn conversation_summary_json(
    conversation: &ChatConversationRecord,
    message_count: usize,
) -> serde_json::Value {
    json!({
        "conversationId": conversation.conversation_id,
        "title": display_title(conversation),
        "model": conversation.model,
        "createdAt": conversation.created_at,
        "updatedAt": conversation.updated_at,
        "messageCount": message_count,
    })
}

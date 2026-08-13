use std::path::Path;

use napi::bindgen_prelude::*;
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::super::super::database;
use super::super::super::ChatConversationRecord;
use super::{create_chat_id, get_chat_conversation};

pub fn fork_conversation(
    database_path: &Path,
    source_conversation_id: &str,
    up_to_response_id: &str,
) -> Result<ChatConversationRecord> {
    let mut connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    let transaction = connection
        .transaction()
        .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    // Load source conversation metadata
    let source = transaction
        .query_row(
            "SELECT conversation_id, title, summary, directory_id, model, last_message_preview, api_profile_name
               FROM chat_conversations
              WHERE conversation_id = ?1
              LIMIT 1",
            params![source_conversation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    let new_conversation_id = create_chat_id("conv");
    let new_id = database::create_snowflake_id();

    // Insert new conversation row, marking it as forked. The forked
    // conversation inherits the source conversation's API profile binding so
    // the continuation keeps routing to the same provider/model.
    transaction.execute(
        "INSERT INTO chat_conversations (
           id,
           conversation_id,
           title,
           summary,
           last_message_preview,
           message_count,
           model,
           api_profile_name,
           last_response_id,
           status,
           directory_id,
           forked_from_conversation_id,
           fork_message_count,
           created_at,
           updated_at
         ) VALUES (
           ?1, ?2, ?3, ?4, ?8, 0, ?5, ?9, '', 'active', ?6, ?7, 0, datetime('now', 'localtime'), datetime('now', 'localtime')
         )",
        params![
            new_id,
            new_conversation_id,
            source.1,  // title
            source.2,  // summary
            source.4,  // model
            source.3,  // directory_id
            source_conversation_id,
            source.5,  // last_message_preview
            source.6,  // api_profile_name
        ],
    )
    .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    // Copy messages from the source conversation. When up_to_response_id is
    // non-empty, only messages up to and including the one with that
    // response_id are copied (supports forking from an intermediate AI
    // message). When empty, all messages are copied (full fork).
    let message_rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = {
        let mut stmt = transaction
            .prepare(
                "SELECT message_id, role, content, model, response_id, status, raw_json, thinking, tool_calls_json,
                        interruption_reason, recovery_outcome
                   FROM chat_messages
                  WHERE conversation_id = ?1
                    AND (?2 = '' OR id <= COALESCE(
                      (SELECT id FROM chat_messages WHERE conversation_id = ?1 AND response_id = ?2 LIMIT 1),
                      (SELECT MAX(id) FROM chat_messages WHERE conversation_id = ?1)
                    ))
                  ORDER BY id ASC",
            )
            .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

        let rows = stmt
            .query_map(params![source_conversation_id, up_to_response_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            })
            .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| database::database_error(database_path, "fork conversation", error))?
    };

    for (index, msg) in message_rows.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO chat_messages (
               id,
               message_id,
               conversation_id,
               role,
               content,
               model,
               response_id,
               status,
               raw_json,
               thinking,
               tool_calls_json,
               interruption_reason,
               recovery_outcome,
               created_at
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, datetime('now', 'localtime')
             )",
                params![
                    database::create_snowflake_id(),
                    create_chat_id(&format!("msg{index}")),
                    new_conversation_id,
                    &msg.1,  // role
                    &msg.2,  // content
                    &msg.3,  // model
                    &msg.4,  // response_id
                    &msg.5,  // status
                    &msg.6,  // raw_json
                    &msg.7,  // thinking
                    &msg.8,  // tool_calls_json
                    &msg.9,  // interruption_reason
                    &msg.10, // recovery_outcome
                ],
            )
            .map_err(|error| database::database_error(database_path, "fork conversation", error))?;
    }

    // Update message count and last_message_preview. The preview reflects
    // the last copied message, which may differ from the source conversation's
    // last message when forking from an intermediate point.
    transaction.execute(
        "UPDATE chat_conversations
            SET message_count = (
                SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1
            ),
            fork_message_count = (
                SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1
            ),
            last_message_preview = (
                SELECT content FROM chat_messages WHERE conversation_id = ?1 ORDER BY id DESC LIMIT 1
            ),
            updated_at = datetime('now', 'localtime')
          WHERE conversation_id = ?1",
        params![new_conversation_id],
    )
    .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    transaction
        .commit()
        .map_err(|error| database::database_error(database_path, "fork conversation", error))?;

    // Re-read from DB to get accurate created_at / updated_at
    get_chat_conversation(database_path, &new_conversation_id)?.ok_or_else(|| {
        database::database_error(
            database_path,
            "fork conversation",
            rusqlite::Error::QueryReturnedNoRows,
        )
    })
}

pub fn truncate_conversation_from_response(
    database_path: &Path,
    conversation_id: &str,
    response_id: &str,
) -> Result<()> {
    let mut connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;
    // Reserve the write transaction before locating the rollback boundary.
    // This prevents a concurrent cancelled-stream commit from invalidating a
    // deferred read snapshot before the first DELETE.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    // Locate either an assistant response or a persisted context-compaction
    // boundary. Boundaries are user messages and must be deleted from their own row.
    let target: Option<(String, String)> = transaction
        .query_row(
            "SELECT id, status FROM chat_messages
              WHERE conversation_id = ?1 AND response_id = ?2
              LIMIT 1",
            params![conversation_id, response_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    let (target_id, target_status) = match target {
        Some(target) => target,
        None => return Ok(()),
    };

    let delete_from = if target_status == "context_compaction" {
        target_id.clone()
    } else {
        // Each normal exchange inserts request messages immediately before the
        // assistant response. Include that request when truncating the exchange.
        preceding_request_id(database_path, &transaction, conversation_id, &target_id)?
            .unwrap_or_else(|| target_id.clone())
    };

    truncate_conversation_from_id(database_path, &transaction, conversation_id, &delete_from)?;

    transaction
        .commit()
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    Ok(())
}

/// Truncate a conversation starting from a persisted message id. This is the
/// rollback boundary for exchanges whose assistant row carries no usable
/// `response_id` — most importantly failed turns, where the persisted user
/// message id is the only reliable anchor for the exchange (the failed
/// assistant row stores an empty response_id).
///
/// When the referenced row is a normal assistant message, its preceding
/// request row is included in the truncation (mirroring
/// `truncate_conversation_from_response`); otherwise (user message, failed
/// exchange user row, or context-compaction boundary) the row itself and
/// everything after it is deleted. No-op when the id does not exist in the
/// conversation (idempotent).
pub fn truncate_conversation_from_message(
    database_path: &Path,
    conversation_id: &str,
    message_id: &str,
) -> Result<()> {
    let mut connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    let target: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT id, role, status FROM chat_messages
              WHERE conversation_id = ?1 AND id = ?2
              LIMIT 1",
            params![conversation_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    let (target_id, target_role, target_status) = match target {
        Some(target) => target,
        None => return Ok(()),
    };

    let delete_from = if target_role == "assistant" && target_status != "context_compaction" {
        // An assistant row (only reachable when the caller passed an assistant
        // id) must include its preceding request row.
        preceding_request_id(database_path, &transaction, conversation_id, &target_id)?
            .unwrap_or_else(|| target_id.clone())
    } else {
        // User rows — including failed-exchange user messages — and
        // context-compaction boundaries are deleted from their own id.
        target_id.clone()
    };

    truncate_conversation_from_id(database_path, &transaction, conversation_id, &delete_from)?;

    transaction
        .commit()
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    Ok(())
}

/// Locate the request row (response_id = '') immediately before the given
/// message id. Returns `None` when no such row exists.
fn preceding_request_id(
    database_path: &Path,
    transaction: &rusqlite::Transaction<'_>,
    conversation_id: &str,
    target_id: &str,
) -> Result<Option<String>> {
    transaction
        .query_row(
            "SELECT id FROM chat_messages
              WHERE conversation_id = ?1 AND id < ?2 AND response_id = ''
              ORDER BY id DESC
              LIMIT 1",
            params![conversation_id, target_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))
}

/// Delete the exchange starting at `delete_from` (todo items, messages, and
/// conversation metadata refresh). Runs inside the caller's write transaction.
fn truncate_conversation_from_id(
    database_path: &Path,
    transaction: &rusqlite::Transaction<'_>,
    conversation_id: &str,
    delete_from: &str,
) -> Result<()> {
    // Delete linked TODO items before deleting their response rows, otherwise the
    // response-id subquery would no longer be able to locate the affected items.
    transaction
        .execute(
            "DELETE FROM todo_items
              WHERE session_id = ?1
                AND response_id IN (
                  SELECT response_id FROM chat_messages
                    WHERE conversation_id = ?1
                      AND response_id <> ''
                      AND id >= ?2
                )",
            params![conversation_id, delete_from],
        )
        .map_err(|error| database::database_error(database_path, "delete todo items", error))?;

    // Delete the selected exchange or boundary and everything after it. Messages
    // before a compaction boundary remain available to full-conversation rollback.
    transaction
        .execute(
            "DELETE FROM chat_messages
              WHERE conversation_id = ?1 AND id >= ?2",
            params![conversation_id, delete_from],
        )
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    // Refresh conversation metadata so the sidebar stays consistent.
    transaction
        .execute(
            "UPDATE chat_conversations
                SET message_count = (
                      SELECT COUNT(*) FROM chat_messages WHERE conversation_id = ?1
                    ),
                    last_message_preview = COALESCE(
                      (SELECT content FROM chat_messages
                        WHERE conversation_id = ?1 ORDER BY id DESC LIMIT 1),
                      ''
                    ),
                    last_response_id = COALESCE(
                      (SELECT response_id FROM chat_messages
                        WHERE conversation_id = ?1 AND response_id <> ''
                        ORDER BY id DESC LIMIT 1),
                      ''
                    ),
                    input_tokens = 0,
                    output_tokens = 0,
                    cache_creation_input_tokens = 0,
                    cache_read_input_tokens = 0,
                    updated_at = datetime('now', 'localtime')
              WHERE conversation_id = ?1",
            params![conversation_id],
        )
        .map_err(|error| database::database_error(database_path, "truncate conversation", error))?;

    Ok(())
}

pub(crate) fn find_conversation_id_by_response_id(
    database_path: &Path,
    response_id: &str,
) -> Result<Option<String>> {
    database::open_connection(database_path)
        .and_then(|connection| {
            connection
                .query_row(
                    "SELECT conversation_id
                       FROM chat_messages
                      WHERE response_id = ?1
                        AND response_id <> ''
                      ORDER BY id DESC
                      LIMIT 1",
                    [response_id],
                    |row| row.get(0),
                )
                .optional()
        })
        .map_err(|error| database::database_error(database_path, "find chat conversation", error))
}

pub(crate) fn conversation_exists(database_path: &Path, conversation_id: &str) -> Result<bool> {
    database::open_connection(database_path)
        .and_then(|connection| {
            connection
                .query_row(
                    "SELECT 1 FROM chat_conversations WHERE conversation_id = ?1 LIMIT 1",
                    [conversation_id],
                    |_| Ok(()),
                )
                .optional()
                .map(|value| value.is_some())
        })
        .map_err(|error| database::database_error(database_path, "check chat conversation", error))
}

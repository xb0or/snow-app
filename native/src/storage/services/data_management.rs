//! Data-management primitives shared by the Electron coordinator.
//!
//! The main process owns package files, passwords and WebDAV requests. This
//! module owns the parts that must happen against SQLite itself: online
//! backup, integrity checks, and transactional configuration export/import.
//! Keeping these operations in the native layer prevents the renderer from
//! ever receiving a live database handle or a secret-bearing SQL result.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use napi::bindgen_prelude::*;
use rusqlite::{backup::Backup, params_from_iter, types::Value as SqlValue, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::super::database;

pub const CONFIG_FORMAT_VERSION: i64 = 1;
pub const REDACTED_MARKER: &str = "__SNOWAPP_REDACTED__";
const MAX_CONFIG_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONFIG_ROWS: usize = 50_000;

const KNOWN_SECTIONS: &[&str] = &[
    "api-config",
    "model-settings",
    "system-settings",
    "mcp",
    "prompts",
    "hooks",
    "sub-agents",
    "keyboard-shortcuts",
    "theme",
    "skills",
    "plugins",
];

const CONFIG_TABLES: &[&str] = &[
    "api_configs",
    "system_settings",
    "system_prompts",
    "custom_header_schemes",
    "mcp_server_configs",
    "plugins",
    "plugin_marketplaces",
    "plugin_components",
    "sub_agent_configs",
    "sensitive_command_configs",
];

#[derive(Clone, Debug)]
pub struct DatabaseBackupInfo {
    pub database_path: String,
    pub archive_database_path: Option<String>,
    pub schema_version: i64,
    pub database_size_bytes: u64,
    pub archive_database_size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigBundle {
    format_version: i64,
    schema_version: i64,
    sections: Vec<String>,
    contains_secrets: bool,
    tables: BTreeMap<String, Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigBundleInput {
    format_version: i64,
    schema_version: i64,
    sections: Vec<String>,
    #[serde(default)]
    contains_secrets: bool,
    tables: BTreeMap<String, Vec<Value>>,
}

fn data_error(message: impl Into<String>) -> Error {
    Error::new(Status::GenericFailure, message.into())
}

fn invalid_data(message: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, message.into())
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| data_error(format!("Cannot determine parent for {}", path.display())))?;
    fs::create_dir_all(parent).map_err(|error| {
        data_error(format!("Failed to create {}: {error}", parent.display()))
    })
}

fn remove_database_sidecars(path: &Path) {
    let _ = fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
    let _ = fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
    let _ = fs::remove_file(PathBuf::from(format!("{}-journal", path.display())));
}

fn open_archive_source(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(Duration::from_secs(10))?;
    Ok(connection)
}

fn quick_check_connection(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("PRAGMA quick_check(1)")
        .map_err(|error| data_error(format!("Failed to prepare SQLite quick_check: {error}")))?;
    let result: String = statement
        .query_row([], |row| row.get(0))
        .map_err(|error| data_error(format!("Failed to run SQLite quick_check: {error}")))?;
    if result != "ok" {
        return Err(data_error(format!("SQLite quick_check failed: {result}")));
    }
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| data_error(format!("Failed to read SQLite schema version: {error}")))
}

fn online_backup(source: &Path, destination: &Path, archive_source: bool) -> Result<(i64, u64)> {
    ensure_parent(destination)?;
    if source == destination {
        return Err(invalid_data("Online backup destination must differ from source"));
    }
    let _ = fs::remove_file(destination);
    remove_database_sidecars(destination);

    let source_connection = if archive_source {
        open_archive_source(source)
            .map_err(|error| data_error(format!("Failed to open archive database: {error}")))?
    } else {
        database::open_connection(source)
            .map_err(|error| data_error(format!("Failed to open live database: {error}")))?
    };
    let source_schema = schema_version(&source_connection)?;
    let mut destination_connection = Connection::open(destination)
        .map_err(|error| data_error(format!("Failed to create backup database: {error}")))?;
    destination_connection
        .busy_timeout(Duration::from_secs(10))
        .map_err(|error| data_error(format!("Failed to configure backup database: {error}")))?;

    {
        let backup = Backup::new(&source_connection, &mut destination_connection)
            .map_err(|error| data_error(format!("Failed to start SQLite online backup: {error}")))?;
        backup
            .run_to_completion(64, Duration::from_millis(10), None)
            .map_err(|error| data_error(format!("SQLite online backup failed: {error}")))?;
    }
    quick_check_connection(&destination_connection)?;
    drop(destination_connection);
    drop(source_connection);

    let size = fs::metadata(destination)
        .map_err(|error| data_error(format!("Failed to stat backup database: {error}")))?
        .len();
    Ok((source_schema, size))
}

pub fn create_database_online_backup(
    main_destination: String,
    archive_destination: Option<String>,
    include_archive: bool,
) -> Result<DatabaseBackupInfo> {
    super::super::with_data_management_lock(|| {
        let main_source = super::super::ensure_database_file()?;
        let (schema, main_size) = online_backup(
            &main_source,
            Path::new(&main_destination),
            false,
        )?;

        let mut archive_path = None;
        let mut archive_size = None;
        if include_archive {
            let destination = archive_destination
                .as_deref()
                .ok_or_else(|| invalid_data("Archive destination is required when enabled"))?;
            let source = super::super::ensure_archive_database_file()?;
            let (_, size) = online_backup(Path::new(&source), Path::new(destination), true)?;
            archive_path = Some(destination.to_string());
            archive_size = Some(size);
        }

        Ok(DatabaseBackupInfo {
            database_path: main_destination,
            archive_database_path: archive_path,
            schema_version: schema,
            database_size_bytes: main_size,
            archive_database_size_bytes: archive_size,
        })
    })
}

pub fn quick_check_database(path: String) -> Result<String> {
    let connection = Connection::open(&path)
        .map_err(|error| data_error(format!("Failed to open database '{}': {error}", path)))?;
    connection
        .busy_timeout(Duration::from_secs(10))
        .map_err(|error| data_error(format!("Failed to configure database check: {error}")))?;
    quick_check_connection(&connection)?;
    Ok("ok".to_string())
}

fn parse_sections(raw: &str) -> Result<BTreeSet<String>> {
    let values: Vec<String> = serde_json::from_str(raw)
        .map_err(|error| invalid_data(format!("Invalid data-management sections: {error}")))?;
    if values.is_empty() {
        return Err(invalid_data("At least one data-management section is required"));
    }
    let mut sections = BTreeSet::new();
    for section in values {
        if !KNOWN_SECTIONS.contains(&section.as_str()) {
            return Err(invalid_data(format!("Unknown data-management section: {section}")));
        }
        sections.insert(section);
    }
    Ok(sections)
}

fn setting_section(setting_code: &str) -> &'static str {
    if setting_code == "theme_settings" {
        "theme"
    } else if setting_code == "keyboard_shortcuts" {
        "keyboard-shortcuts"
    } else if setting_code == "hooks_global" || setting_code.starts_with("hooks_project_") {
        "hooks"
    } else if setting_code == "mcp_global_scope"
        || setting_code.starts_with("project_mcp_")
    {
        "mcp"
    } else if setting_code.starts_with("project_skills_scope_") {
        "skills"
    } else if setting_code == "codebase_settings" || setting_code == "privacy_settings" {
        "model-settings"
    } else {
        "system-settings"
    }
}

fn table_allowed(table: &str, sections: &BTreeSet<String>) -> bool {
    if !CONFIG_TABLES.contains(&table) {
        return false;
    }
    match table {
        "api_configs" => sections.contains("api-config"),
        "system_prompts" => sections.contains("prompts"),
        "mcp_server_configs" => sections.contains("mcp"),
        "custom_header_schemes" => sections.contains("api-config"),
        "sub_agent_configs" => sections.contains("sub-agents"),
        "plugins" | "plugin_marketplaces" | "plugin_components" => sections.contains("plugins"),
        "sensitive_command_configs" => sections.contains("system-settings"),
        "system_settings" => true,
        _ => false,
    }
}

fn table_names(sections: &BTreeSet<String>) -> Vec<&'static str> {
    CONFIG_TABLES
        .iter()
        .copied()
        .filter(|table| table_allowed(table, sections))
        .collect()
}

fn is_sensitive_column(column: &str) -> bool {
    matches!(
        column,
        "api_key" | "vision_api_key" | "env_json" | "headers_json"
    )
}

fn is_device_only_column(column: &str) -> bool {
    matches!(
        column,
        "source_path"
            | "manifest_path"
            | "cache_path"
            | "target_path"
            | "origin_path"
            | "path"
    )
}

fn key_is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.ends_with("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("authorization")
        || key.contains("cookie")
        || key == "env"
        || key == "headers"
}

fn key_is_device_only(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "path"
        || key.ends_with("path")
        || key.contains("workspace")
        || key.contains("ssh")
        || key.contains("credential")
}

fn sanitize_json(value: Value, key: Option<&str>, include_secrets: bool) -> Value {
    if key.is_some_and(key_is_device_only) {
        return Value::String(REDACTED_MARKER.to_string());
    }
    if key.is_some_and(|key| key_is_secret(key) && !include_secrets) {
        return Value::String(REDACTED_MARKER.to_string());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let sanitized = sanitize_json(value, Some(&key), include_secrets);
                    (key, sanitized)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| sanitize_json(value, None, include_secrets))
                .collect(),
        ),
        other => other,
    }
}

fn sql_value_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(value) => Value::Number(value.into()),
        SqlValue::Real(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SqlValue::Text(value) => Value::String(value),
        SqlValue::Blob(value) => Value::String(format!("base64:{}", BASE64.encode(value))),
    }
}

fn value_for_export(
    table: &str,
    column: &str,
    value: Value,
    row: &Map<String, Value>,
    include_secrets: bool,
) -> Option<Value> {
    if is_device_only_column(column) {
        return None;
    }
    if is_sensitive_column(column) && !include_secrets {
        return None;
    }
    if table == "system_settings" && column == "setting_value" {
        let setting_code = row
            .get("setting_code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if setting_code.ends_with("_dir") || setting_code == "image_library_dir" {
            return Some(Value::String(REDACTED_MARKER.to_string()));
        }
        if let Value::String(raw) = &value {
            if let Ok(json) = serde_json::from_str::<Value>(raw) {
                let sanitized = sanitize_json(json, None, include_secrets);
                return Some(Value::String(
                    serde_json::to_string(&sanitized).unwrap_or_else(|_| raw.clone()),
                ));
            }
        }
    }
    if column.ends_with("_json") || column == "config_json" {
        if let Value::String(raw) = &value {
            if let Ok(json) = serde_json::from_str::<Value>(raw) {
                let sanitized = sanitize_json(json, None, include_secrets);
                return Some(Value::String(
                    serde_json::to_string(&sanitized).unwrap_or_else(|_| raw.clone()),
                ));
            }
        }
    }
    Some(value)
}

fn read_table_rows(
    connection: &Connection,
    table: &str,
    sections: &BTreeSet<String>,
    include_secrets: bool,
) -> Result<Vec<Value>> {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM \"{table}\""))
        .map_err(|error| data_error(format!("Failed to read configuration table {table}: {error}")))?;
    let column_names = (0..statement.column_count())
        .map(|index| statement.column_name(index).unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mapped = statement
        .query_map([], |row| {
            let mut raw = Map::new();
            for (index, column) in column_names.iter().enumerate() {
                raw.insert(column.clone(), sql_value_to_json(row.get(index)?));
            }
            Ok(raw)
        })
        .map_err(|error| data_error(format!("Failed to enumerate configuration table {table}: {error}")))?;
    for row in mapped {
        let raw = row.map_err(|error| data_error(format!("Failed to read configuration row: {error}")))?;
        if table == "system_settings" {
            let code = raw
                .get("setting_code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !sections.contains(setting_section(code)) {
                continue;
            }
        }
        let mut sanitized = Map::new();
        for (column, value) in &raw {
            if let Some(value) = value_for_export(
                table,
                column,
                value.clone(),
                &raw,
                include_secrets,
            ) {
                sanitized.insert(column.clone(), value);
            }
        }
        rows.push(Value::Object(sanitized));
        if rows.len() > MAX_CONFIG_ROWS {
            return Err(data_error("Configuration export exceeds the row limit"));
        }
    }
    Ok(rows)
}

pub fn export_config_data(sections_json: String, include_secrets: bool) -> Result<String> {
    super::super::with_data_management_lock(|| {
        let sections = parse_sections(&sections_json)?;
        let database_path = super::super::ensure_database_file()?;
        let connection = database::open_connection(&database_path)
            .map_err(|error| data_error(format!("Failed to open configuration database: {error}")))?;
        let mut tables = BTreeMap::new();
        for table in table_names(&sections) {
            let rows = read_table_rows(&connection, table, &sections, include_secrets)?;
            // Empty tables are part of the contract: replacement imports need
            // to distinguish "selected and empty" from "not exported".
            tables.insert(table.to_string(), rows);
        }
        let bundle = ConfigBundle {
            format_version: CONFIG_FORMAT_VERSION,
            schema_version: schema_version(&connection)?,
            sections: sections.into_iter().collect(),
            contains_secrets: include_secrets,
            tables,
        };
        let encoded = serde_json::to_string(&bundle)
            .map_err(|error| data_error(format!("Failed to serialize configuration export: {error}")))?;
        if encoded.len() > MAX_CONFIG_BYTES {
            return Err(data_error("Configuration export exceeds the size limit"));
        }
        Ok(encoded)
    })
}

fn table_columns(connection: &Connection, table: &str) -> Result<(Vec<String>, String)> {
    if !CONFIG_TABLES.contains(&table) {
        return Err(invalid_data(format!("Configuration table is not allowed: {table}")));
    }
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|error| data_error(format!("Failed to inspect configuration table {table}: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let primary: i64 = row.get(5)?;
            Ok((name, primary))
        })
        .map_err(|error| data_error(format!("Failed to inspect configuration table {table}: {error}")))?;
    let mut columns = Vec::new();
    let mut primary = String::new();
    for row in rows {
        let (name, is_primary) = row.map_err(|error| data_error(format!("Failed to inspect table: {error}")))?;
        if is_primary > 0 && primary.is_empty() {
            primary = name.clone();
        }
        columns.push(name);
    }
    if columns.is_empty() || primary.is_empty() {
        return Err(data_error(format!("Configuration table {table} has no usable primary key")));
    }
    Ok((columns, primary))
}

fn conflict_column<'a>(table: &str, primary: &'a str) -> &'a str {
    match table {
        "system_settings" => "setting_code",
        "api_configs" => "profile_name",
        "system_prompts" => "prompt_id",
        "custom_header_schemes" => "scheme_id",
        "mcp_server_configs" => "server_id",
        _ => primary,
    }
}

fn contains_redacted(value: &Value) -> bool {
    match value {
        Value::String(value) => value == REDACTED_MARKER,
        Value::Array(values) => values.iter().any(contains_redacted),
        Value::Object(values) => values.values().any(contains_redacted),
        _ => false,
    }
}

fn merge_redacted(existing: Value, incoming: Value) -> Value {
    if incoming == Value::String(REDACTED_MARKER.to_string()) {
        return existing;
    }
    match (existing, incoming) {
        (Value::Object(mut existing), Value::Object(incoming)) => {
            for (key, value) in incoming {
                if value == Value::String(REDACTED_MARKER.to_string()) {
                    continue;
                }
                let merged = existing
                    .remove(&key)
                    .map(|old| merge_redacted(old, value.clone()))
                    .unwrap_or(value);
                existing.insert(key, merged);
            }
            Value::Object(existing)
        }
        (_, incoming) => incoming,
    }
}

fn row_current_values(
    connection: &Connection,
    table: &str,
    key_column: &str,
    key_value: &Value,
) -> Result<Option<Map<String, Value>>> {
    let sql = format!("SELECT * FROM \"{table}\" WHERE \"{key_column}\" = ?1 LIMIT 1");
    let parameter = json_to_sql_value(key_value)?;
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| data_error(format!("Failed to inspect existing {table} row: {error}")))?;
    let columns = (0..statement.column_count())
        .map(|index| statement.column_name(index).unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    let mut rows = statement
        .query(params_from_iter([parameter]))
        .map_err(|error| data_error(format!("Failed to inspect existing {table} row: {error}")))?;
    let Some(row) = rows
        .next()
        .map_err(|error| data_error(format!("Failed to inspect existing {table} row: {error}")))?
    else {
        return Ok(None);
    };
    let mut values = Map::new();
    for (index, column) in columns.iter().enumerate() {
        values.insert(column.clone(), sql_value_to_json(row.get(index).map_err(|error| {
            data_error(format!("Failed to read existing {table} row: {error}"))
        })?));
    }
    Ok(Some(values))
}

fn json_to_sql_value(value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(i64::from(*value)),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                SqlValue::Integer(integer)
            } else if let Some(float) = value.as_f64() {
                SqlValue::Real(float)
            } else {
                return Err(invalid_data("Unsupported JSON number in configuration import"));
            }
        }
        Value::String(value) if value.starts_with("base64:") => BASE64
            .decode(value.trim_start_matches("base64:"))
            .map(SqlValue::Blob)
            .map_err(|error| invalid_data(format!("Invalid configuration blob: {error}")))?,
        Value::String(value) => SqlValue::Text(value.clone()),
        _ => return Err(invalid_data("Configuration import values must be scalar")),
    })
}

fn delete_missing_rows(
    connection: &Connection,
    table: &str,
    rows: &[Value],
    primary: &str,
    sections: &BTreeSet<String>,
) -> Result<()> {
    if table == "system_settings" {
        let mut statement = connection
            .prepare("SELECT setting_code FROM system_settings")
            .map_err(|error| data_error(format!("Failed to inspect system settings: {error}")))?;
        let existing_codes: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .map_err(|error| data_error(format!("Failed to inspect system settings: {error}")))?
            .filter_map(|row| row.ok())
            .filter(|code: &String| sections.contains(setting_section(code)))
            .collect();
        let incoming: BTreeSet<String> = rows
            .iter()
            .filter_map(|row| row.get("setting_code").and_then(Value::as_str).map(str::to_string))
            .collect();
        for code in existing_codes {
            if !incoming.contains(&code) {
                connection
                    .execute("DELETE FROM system_settings WHERE setting_code = ?1", [&code])
                    .map_err(|error| data_error(format!("Failed to replace system setting: {error}")))?;
            }
        }
        return Ok(());
    }
    let keys: Vec<Value> = rows
        .iter()
        .filter_map(|row| row.get(primary).cloned())
        .collect();
    if keys.is_empty() {
        connection
            .execute(&format!("DELETE FROM \"{table}\""), [])
            .map_err(|error| data_error(format!("Failed to replace configuration table {table}: {error}")))?;
        return Ok(());
    }
    let existing_keys: Vec<SqlValue> = keys
        .iter()
        .map(json_to_sql_value)
        .collect::<Result<Vec<_>>>()?;
    let placeholders = std::iter::repeat("?")
        .take(existing_keys.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("DELETE FROM \"{table}\" WHERE \"{primary}\" NOT IN ({placeholders})");
    connection
        .execute(&sql, params_from_iter(existing_keys))
        .map_err(|error| data_error(format!("Failed to replace configuration table {table}: {error}")))?;
    Ok(())
}

pub fn apply_config_data(
    config_json: String,
    sections_json: String,
    replace_selected: bool,
) -> Result<()> {
    if config_json.len() > MAX_CONFIG_BYTES {
        return Err(invalid_data("Configuration package is too large"));
    }
    super::super::with_data_management_lock(|| {
        let requested_sections = parse_sections(&sections_json)?;
        let bundle: ConfigBundleInput = serde_json::from_str(&config_json)
            .map_err(|error| invalid_data(format!("Invalid configuration bundle: {error}")))?;
        if bundle.format_version != CONFIG_FORMAT_VERSION {
            return Err(invalid_data(format!(
                "Unsupported configuration format version: {}",
                bundle.format_version
            )));
        }
        let bundle_sections = bundle.sections.iter().cloned().collect::<BTreeSet<_>>();
        for section in &bundle_sections {
            if !KNOWN_SECTIONS.contains(&section.as_str()) {
                return Err(invalid_data(format!("Unknown configuration section: {section}")));
            }
        }
        for section in &requested_sections {
            if !bundle_sections.contains(section) {
                return Err(invalid_data(format!(
                    "Configuration package does not contain selected section: {section}"
                )));
            }
        }
        let total_rows = bundle.tables.values().map(Vec::len).sum::<usize>();
        if total_rows > MAX_CONFIG_ROWS {
            return Err(invalid_data("Configuration package exceeds the row limit"));
        }

        let database_path = super::super::ensure_database_file()?;
        let mut connection = database::open_connection(&database_path)
            .map_err(|error| data_error(format!("Failed to open configuration database: {error}")))?;
        let current_schema = schema_version(&connection)?;
        if bundle.schema_version > current_schema {
            return Err(invalid_data(format!(
                "Configuration package schema {} is newer than supported schema {current_schema}",
                bundle.schema_version
            )));
        }
        let transaction = connection
            .transaction()
            .map_err(|error| data_error(format!("Failed to start configuration transaction: {error}")))?;

        for (table, rows) in &bundle.tables {
            if !table_allowed(table, &bundle_sections) {
                return Err(invalid_data(format!(
                    "Configuration table is outside the package sections: {table}"
                )));
            }
            if !table_allowed(table, &requested_sections) {
                continue;
            }
            let (columns, primary) = table_columns(&transaction, table)?;
            let key_column = conflict_column(table, &primary);
            for row in rows {
                let object = row
                    .as_object()
                    .ok_or_else(|| invalid_data(format!("Configuration row in {table} is not an object")))?;
                if table == "system_settings" {
                    let setting_code = object
                        .get("setting_code")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let section = setting_section(setting_code);
                    if !bundle_sections.contains(section) {
                        return Err(invalid_data(format!(
                            "System setting is outside the package sections: {setting_code}"
                        )));
                    }
                    if !requested_sections.contains(section) {
                        continue;
                    }
                }
                let key_value = object
                    .get(key_column)
                    .or_else(|| object.get(&primary))
                    .ok_or_else(|| invalid_data(format!("Configuration row in {table} has no identity")))?;
                let current = row_current_values(&transaction, table, key_column, key_value)?;
                let mut row_values = Vec::new();
                for (column, incoming) in object {
                    if !columns.contains(column) || is_device_only_column(column) {
                        continue;
                    }
                    let mut value = incoming.clone();
                    if contains_redacted(&value) {
                        if let Some(existing) = current.as_ref().and_then(|row| row.get(column)) {
                            if let (Value::String(existing), Value::String(incoming)) = (existing, &value) {
                                if let (Ok(existing_json), Ok(incoming_json)) = (
                                    serde_json::from_str::<Value>(existing),
                                    serde_json::from_str::<Value>(incoming),
                                ) {
                                    value = Value::String(
                                        serde_json::to_string(&merge_redacted(existing_json, incoming_json))
                                            .map_err(|error| data_error(format!("Failed to merge redacted configuration: {error}")))?,
                                    );
                                } else if incoming == REDACTED_MARKER {
                                    if table == "system_settings" && column == "setting_value" && current.is_none() {
                                        value = Value::String(String::new());
                                    } else {
                                        continue;
                                    }
                                }
                            } else if value == Value::String(REDACTED_MARKER.to_string()) {
                                if table == "system_settings" && column == "setting_value" && current.is_none() {
                                    value = Value::String(String::new());
                                } else {
                                    continue;
                                }
                            }
                        } else if value == Value::String(REDACTED_MARKER.to_string()) {
                            if table == "system_settings" && column == "setting_value" {
                                value = Value::String(String::new());
                            } else {
                                continue;
                            }
                        }
                    }
                    row_values.push((column.clone(), json_to_sql_value(&value)?));
                }
                let update_columns = row_values
                    .iter()
                    .map(|(column, _)| column.clone())
                    .collect::<Vec<_>>();
                if table == "plugins" {
                    if !row_values.iter().any(|(column, _)| column == "source_path") {
                        row_values.push(("source_path".to_string(), SqlValue::Text(String::new())));
                    }
                    if !row_values.iter().any(|(column, _)| column == "manifest_path") {
                        row_values.push(("manifest_path".to_string(), SqlValue::Text(String::new())));
                    }
                } else if table == "plugin_components"
                    && !row_values.iter().any(|(column, _)| column == "origin_path")
                {
                    row_values.push(("origin_path".to_string(), SqlValue::Text(String::new())));
                } else if table == "plugin_marketplaces" {
                    if !row_values.iter().any(|(column, _)| column == "source_path") {
                        row_values.push(("source_path".to_string(), SqlValue::Text(String::new())));
                    }
                    if !row_values.iter().any(|(column, _)| column == "manifest_path") {
                        row_values.push(("manifest_path".to_string(), SqlValue::Text(String::new())));
                    }
                }
                if row_values.is_empty() {
                    continue;
                }
                let names = row_values
                    .iter()
                    .map(|(name, _)| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let placeholders = std::iter::repeat("?")
                    .take(row_values.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let updates = row_values
                    .iter()
                    .filter(|(name, _)| update_columns.iter().any(|column| column == name))
                    .map(|(name, _)| format!("\"{name}\" = excluded.\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "INSERT INTO \"{table}\" ({names}) VALUES ({placeholders}) ON CONFLICT(\"{key_column}\") DO UPDATE SET {updates}"
                );
                let values = row_values.into_iter().map(|(_, value)| value).collect::<Vec<_>>();
                transaction
                    .execute(&sql, params_from_iter(values))
                    .map_err(|error| data_error(format!("Failed to import configuration row in {table}: {error}")))?;
            }
            if replace_selected {
                delete_missing_rows(&transaction, table, rows, &primary, &requested_sections)?;
            }
        }
        transaction
            .commit()
            .map_err(|error| data_error(format!("Failed to commit configuration import: {error}")))?;
        Ok(())
    })
}

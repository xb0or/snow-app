use std::fs;
use std::path::Path;

use napi::bindgen_prelude::*;
use napi_derive::napi;

mod browse;
mod mutate;
mod search;

pub use self::browse::*;
pub use self::mutate::*;
pub use self::search::*;

#[napi(object)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: i64,
}

#[napi(object)]
pub struct FileContentResult {
    pub content: String,
    pub is_binary: bool,
    pub is_image: bool,
    pub is_svg: bool,
    pub mime_type: String,
    pub encoding: String,
    pub size: i64,
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "svg"];

const MIME_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("svg", "image/svg+xml"),
];

fn get_mime_type(ext: &str) -> String {
    for (e, mime) in MIME_TYPES {
        if *e == ext {
            return mime.to_string();
        }
    }
    "application/octet-stream".to_string()
}

pub fn process_file_content(file_path: &str, buffer: Vec<u8>) -> FileContentResult {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    let is_svg = ext == "svg";
    let is_image = IMAGE_EXTENSIONS.contains(&ext.as_str());
    let size = buffer.len() as i64;

    if is_svg {
        return FileContentResult {
            content: String::from_utf8_lossy(&buffer).into_owned(),
            is_binary: false,
            is_image: true,
            is_svg: true,
            mime_type: "image/svg+xml".to_string(),
            encoding: "utf8".to_string(),
            size,
        };
    }

    if is_image {
        return FileContentResult {
            content: base64_encode(&buffer),
            is_binary: true,
            is_image: true,
            is_svg: false,
            mime_type: get_mime_type(&ext),
            encoding: "base64".to_string(),
            size,
        };
    }

    let check_len = buffer.len().min(8192);
    let is_binary = buffer[..check_len].iter().any(|&b| b == 0);

    if is_binary {
        return FileContentResult {
            content: base64_encode(&buffer),
            is_binary: true,
            is_image: false,
            is_svg: false,
            mime_type: "application/octet-stream".to_string(),
            encoding: "base64".to_string(),
            size,
        };
    }

    FileContentResult {
        content: String::from_utf8_lossy(&buffer).into_owned(),
        is_binary: false,
        is_image: false,
        is_svg: false,
        mime_type: "text/plain".to_string(),
        encoding: "utf8".to_string(),
        size,
    }
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

pub fn read_file_content(file_path: &str) -> Result<FileContentResult> {
    let path = Path::new(file_path);

    if !path.exists() {
        return Err(Error::from_reason(format!(
            "File does not exist: {}",
            file_path
        )));
    }

    if !path.is_file() {
        return Err(Error::from_reason(format!(
            "Path is not a file: {}",
            file_path
        )));
    }

    let buffer = fs::read(path)
        .map_err(|e| Error::from_reason(format!("Failed to read file '{}': {}", file_path, e)))?;

    Ok(process_file_content(file_path, buffer))
}

/// Write text content to a file, creating it (and parent directories) if missing.
pub fn write_file_content(file_path: &str, content: &str) -> Result<()> {
    let path = Path::new(file_path);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::from_reason(format!(
                    "Failed to create parent directories for '{}': {}",
                    file_path, e
                ))
            })?;
        }
    }

    fs::write(path, content.as_bytes())
        .map_err(|e| Error::from_reason(format!("Failed to write file '{}': {}", file_path, e)))
}

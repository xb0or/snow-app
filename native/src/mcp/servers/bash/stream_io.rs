use super::*;

use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Kill the entire process tree rooted at `child`, not just the
/// immediate shell process. On Windows, `taskkill` is launched asynchronously
/// and bounded by a hard deadline; if it stalls, the shell is force-killed
/// immediately as a fallback. On Unix the dedicated process group is killed.
pub(crate) async fn kill_process_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(target_os = "windows")]
        {
            // /T = kill entire process tree, /F = force kill. Do not await this
            // command indefinitely: a broken taskkill must never block the
            // safety-critical cancellation path.
            let killer = crate::utils::process::cmd_async("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            if let Ok(mut killer) = killer {
                let _ = tokio::time::timeout(Duration::from_millis(750), killer.wait()).await;
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Negative PID kills the entire process group. The child was
            // spawned with process_group(0), so it leads its own group.
            let _ = tokio::process::Command::new("kill")
                .args(["-9", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }

    // Fallback is non-blocking. The bounded wait only reaps the direct child;
    // it can never keep the Electron event loop blocked.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_millis(750), child.wait()).await;
}

pub(crate) async fn read_stream<R>(
    mut reader: R,
    stream: &'static str,
    on_chunk: Arc<BashStreamCallback>,
    first_output_ms: Arc<OnceLock<u64>>,
    execution_started: Instant,
) -> String
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut pending_utf8 = Vec::new();

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };

        let _ = first_output_ms.set(execution_started.elapsed().as_millis() as u64);
        output.extend_from_slice(&buffer[..read]);
        pending_utf8.extend_from_slice(&buffer[..read]);
        emit_complete_utf8_chunks(&on_chunk, stream, &mut pending_utf8);
    }

    if !pending_utf8.is_empty() {
        emit_stream_chunk(
            &on_chunk,
            stream,
            String::from_utf8_lossy(&pending_utf8).into_owned(),
        );
    }

    strip_ansi_codes(&String::from_utf8_lossy(&output))
}

fn emit_complete_utf8_chunks(on_chunk: &BashStreamCallback, stream: &str, pending: &mut Vec<u8>) {
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                emit_stream_chunk(on_chunk, stream, text.to_string());
                pending.clear();
                return;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let text = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
                    emit_stream_chunk(on_chunk, stream, text);
                    pending.drain(..valid_up_to);
                    continue;
                }

                if error.error_len().is_none() {
                    return;
                }

                let invalid_len = error.error_len().unwrap_or(1);
                let invalid = String::from_utf8_lossy(&pending[..invalid_len]).into_owned();
                emit_stream_chunk(on_chunk, stream, invalid);
                pending.drain(..invalid_len);
            }
        }
    }
}

pub(crate) fn emit_stream_chunk(on_chunk: &BashStreamCallback, stream: &str, data: String) {
    if data.is_empty() {
        return;
    }

    let cleaned = strip_ansi_codes(&data);
    if cleaned.is_empty() {
        return;
    }

    on_chunk.call(
        BashStreamChunk {
            stream: stream.to_string(),
            data: cleaned,
        },
        ThreadsafeFunctionCallMode::NonBlocking,
    );
}

/// Strip ANSI escape sequences (CSI/SGR color codes, cursor movement,
/// OSC hyperlinks, etc.) from terminal output. These codes are emitted
/// by tools like `vite build` / `npm run build` when they detect a TTY
/// and would otherwise leak as raw `\x1b[...m` bytes into the model
/// context and the UI.
fn strip_ansi_codes(input: &str) -> String {
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    let re = ANSI_RE.get_or_init(|| {
        // CSI sequences: ESC [ ... final byte in 0x40..=0x7E
        // OSC sequences: ESC ] ... BEL  or  ESC ] ... ESC \  (ST)
        // Other two-byte escapes (ESC + single char) that some tools emit.
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9AB]")
            .expect("invalid ANSI strip regex")
    });
    re.replace_all(input, "").into_owned()
}

//! PTY 配下で対話モードの claude を動かす terminal session (cc-webreview-ext#18)。
//!
//! `-p` (headless) は権限承認プロンプトに応答できないため、side panel に xterm.js を
//! 埋め込み、host が portable-pty (Windows: ConPTY) で claude を対話モードのまま
//! spawn して生バイト列を中継する。
//!
//! - 出力は `{type:"term_out", data}` (base64)。PTY チャンクは UTF-8 多バイト文字を
//!   分断し得るため、生の JSON 文字列では運べない。
//! - 入力は `{cmd:"term_input", data}` (xterm.js の onData 文字列をそのまま UTF-8 で書く)。
//! - 終了は `{type:"term_exit", code}` (reader スレッドが EOF で確定させる)。
//! - 「同時 claude 1 本」規約は -p セッションと共通 (main.rs 側で排他)。

use crate::debuglog::DebugLog;
use crate::nmhost::SharedWriter;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// `{cmd:"term_start"}` の入力。
#[derive(Debug, PartialEq)]
pub struct TermStart {
    pub cols: u16,
    pub rows: u16,
    pub chrome: bool,
    pub extra_args: Vec<String>,
    pub cwd: Option<String>,
}

/// `{cmd:"term_start"}` を parse する (純関数)。cols/rows は無指定なら 80x24。
pub fn parse_term_start(v: &Value) -> TermStart {
    let dim = |k: &str, default: u16| {
        v.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    TermStart {
        cols: dim("cols", 80),
        rows: dim("rows", 24),
        chrome: v.get("chrome").and_then(Value::as_bool).unwrap_or(false),
        extra_args: v
            .get("extra_args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        cwd: v
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
    }
}

/// 進行中の terminal session。
pub struct TermSession {
    /// kill 用 (child 本体は exit code 確定のため reader スレッドと共有)。
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// 終了確認 (try_wait) 用。reader スレッドが EOF 後に wait して code を確定させる。
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
}

impl TermSession {
    /// まだ走っているか。
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }

    /// PTY child を kill する (best-effort)。
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
        let _ = self.child.lock().unwrap().wait();
    }

    /// xterm.js の onData 文字列を PTY に書く。
    pub fn write_input(&mut self, data: &str) -> Result<(), String> {
        self.writer
            .write_all(data.as_bytes())
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("term 入力書き込み失敗: {e}"))
    }

    /// 端末サイズ変更。
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

/// claude を PTY 配下 (対話モード) で spawn し、出力中継スレッドを立てる。
pub fn spawn_terminal<W: Write + Send + 'static>(
    claude: &Path,
    req: &TermStart,
    writer: &Arc<SharedWriter<W>>,
    log: &Arc<DebugLog>,
) -> Result<TermSession, String> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: req.rows,
            cols: req.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty 失敗: {e}"))?;

    let mut cmd = command_builder_for(claude);
    if req.chrome {
        cmd.arg("--chrome");
    }
    for a in &req.extra_args {
        cmd.arg(a);
    }
    if let Some(cwd) = &req.cwd {
        cmd.cwd(cwd);
    }
    cmd.env("TERM", "xterm-256color");

    let child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("claude spawn 失敗 ({}): {e}", claude.display()))?;
    drop(pty.slave);

    let killer = child.clone_killer();
    let child = Arc::new(Mutex::new(child));
    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| format!("PTY reader 取得失敗: {e}"))?;
    let pty_writer = pty
        .master
        .take_writer()
        .map_err(|e| format!("PTY writer 取得失敗: {e}"))?;

    // 出力中継: PTY → base64 → {type:"term_out"}。EOF で {type:"term_exit", code}。
    // term_out は量が多いので debug.sqlite には byte 数だけ note する (全 dump しない)。
    {
        let w = Arc::clone(writer);
        let l = Arc::clone(log);
        let c = Arc::clone(&child);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut total: u64 = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF / PTY closed
                    Ok(n) => {
                        total += n as u64;
                        let msg = json!({
                            "type": "term_out",
                            "data": B64.encode(&buf[..n]),
                        });
                        if w.send(&msg).is_err() {
                            break; // Chrome 側が閉じた
                        }
                    }
                }
            }
            let code = c
                .lock()
                .unwrap()
                .wait()
                .ok()
                .map(|st| st.exit_code())
                .unwrap_or(0);
            l.note("term_exit", &format!("code={code} bytes={total}"));
            let _ = w.send(&json!({ "type": "term_exit", "code": code }));
        });
    }

    Ok(TermSession {
        killer,
        child,
        writer: pty_writer,
        master: pty.master,
    })
}

/// `.cmd` / `.bat` (npm shim) は直接 spawn できないので cmd /C 経由にする
/// (session.rs の command_for と同じ判定)。
fn command_builder_for(claude: &Path) -> CommandBuilder {
    let ext = claude
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "cmd" || ext == "bat" {
        let mut c = CommandBuilder::new("cmd");
        c.arg("/C");
        c.arg(claude);
        c
    } else {
        CommandBuilder::new(claude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // terminal 実プロセステスト (unix のみ) 専用 import。Windows ビルドでは
    // unused import になるため cfg で絞る。
    #[cfg(unix)]
    use crate::nmhost::read_message;
    #[cfg(unix)]
    use std::io::Cursor;

    #[test]
    fn parse_term_start_defaults_and_full() {
        let d = parse_term_start(&json!({ "cmd": "term_start" }));
        assert_eq!((d.cols, d.rows), (80, 24));
        assert!(!d.chrome);
        assert!(d.extra_args.is_empty());
        assert!(d.cwd.is_none());

        let f = parse_term_start(&json!({
            "cmd": "term_start", "cols": 120, "rows": 40, "chrome": true,
            "extra_args": ["--resume"], "cwd": "C:\\work",
        }));
        assert_eq!((f.cols, f.rows), (120, 40));
        assert!(f.chrome);
        assert_eq!(f.extra_args, vec!["--resume"]);
        assert_eq!(f.cwd.as_deref(), Some("C:\\work"));
    }

    #[test]
    fn parse_term_start_rejects_zero_and_oversize() {
        let v = parse_term_start(&json!({ "cols": 0, "rows": 999999 }));
        assert_eq!((v.cols, v.rows), (80, 24));
    }

    /// spawn → PTY 出力の base64 中継 → term_exit の縦切りを sh で確認する
    /// (CCoW / Linux で走る実プロセステスト。Windows の ConPTY 実走は実機のみ)。
    #[cfg(unix)]
    #[test]
    fn terminal_relays_output_and_exit() {
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let writer = Arc::new(SharedWriter::new(buf.clone()));
        let log = Arc::new(DebugLog::disabled());

        // 入力を 1 行読んでそのまま echo するスクリプト (入出力の round-trip 検証)。
        let req = TermStart {
            cols: 80,
            rows: 24,
            chrome: false,
            extra_args: vec!["-c".into(), "read line; echo \"got:$line\"".into()],
            cwd: None,
        };
        let mut session = spawn_terminal(Path::new("/bin/sh"), &req, &writer, &log).unwrap();
        assert!(session.is_running());
        session.resize(100, 30); // panic しないこと
        session.write_input("hello\r").unwrap();

        // reader スレッドが term_exit を出すまで待つ (上限 5s)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let bytes = buf.0.lock().unwrap().clone();
            if String::from_utf8_lossy(&bytes).contains("term_exit") {
                break;
            }
            assert!(deadline > std::time::Instant::now(), "term_exit が来ない");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let bytes = buf.0.lock().unwrap().clone();
        let mut cursor = Cursor::new(bytes);
        let mut out = Vec::new();
        let mut exit_code = None;
        while let Some(m) = read_message(&mut cursor).unwrap() {
            match m["type"].as_str() {
                Some("term_out") => {
                    let decoded = B64.decode(m["data"].as_str().unwrap()).unwrap();
                    out.extend_from_slice(&decoded);
                }
                Some("term_exit") => exit_code = m["code"].as_u64(),
                _ => {}
            }
        }
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("got:hello"), "PTY round-trip 失敗: {text}");
        assert_eq!(exit_code, Some(0));
        assert!(!session.is_running());
        session.kill(); // 二重 kill しても panic しない
    }
}

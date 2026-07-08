//! claude spawn + stream-json 中継 (cc-webreview-ext#3)。
//!
//! `{cmd:"start"}` で claude を `-p --output-format stream-json --verbose` で spawn し、
//! stdout の JSONL を 1 行 = 1 message (`{type:"claude", data}`) として拡張へ中継する。
//! stderr は `{type:"stderr"}`、終了は `{type:"proc", event:"exit", code, session_id}`。
//!
//! - 同時セッションは 1 本のみ (`{type:"busy"}` で二重起動拒否 — Claude in Chrome 連携の
//!   named pipe 競合対策)。
//! - port 切断 (EOF) 時は claude を kill し、直近 session_id を state ファイルに永続化して
//!   後日 `--resume` できるようにする (ゾンビ claude.exe を残さない)。

use crate::debuglog::DebugLog;
use crate::nmhost::SharedWriter;
use crate::register;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// `{cmd:"start"}` の入力。拡張から受けた JSON を検証済みの形に落とす。
#[derive(Debug, PartialEq)]
pub struct StartRequest {
    pub prompt: String,
    pub chrome: bool,
    pub extra_args: Vec<String>,
    pub cwd: Option<String>,
}

/// 拡張 → host の制御メッセージ。
#[derive(Debug, PartialEq)]
pub enum HostCommand {
    Ping,
    Start(StartRequest),
    Stop,
    /// 手動更新チェック (#6)。結果は {type:"update_status"} で返す。
    CheckUpdate,
    Unknown(String),
}

/// 制御メッセージの parse (純関数)。
pub fn parse_command(v: &Value) -> HostCommand {
    match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
        "ping" => HostCommand::Ping,
        "stop" => HostCommand::Stop,
        "check_update" => HostCommand::CheckUpdate,
        "start" => {
            let prompt = v
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let chrome = v.get("chrome").and_then(Value::as_bool).unwrap_or(false);
            let extra_args = v
                .get("extra_args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let cwd = v
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            HostCommand::Start(StartRequest {
                prompt,
                chrome,
                extra_args,
                cwd,
            })
        }
        other => HostCommand::Unknown(other.to_string()),
    }
}

/// claude の引数を組み立てる純関数。stream-json 前提の固定部 + オプション。
///
/// **prompt は argv に入れない** (stdin で渡す)。argv 渡しは (1) `.cmd` shim の
/// `cmd /C` が改行入り引数を分断する、(2) `-` で始まる行が claude の option parser に
/// `unknown option` として食われる、の 2 経路で複数行プロンプトが壊れる (#4 実機で
/// `error: unknown option '--chrome で…'` を観測)。`claude -p` は stdin が pipe の時
/// stdin 全体をプロンプトとして読む。
pub fn build_claude_args(req: &StartRequest) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if req.chrome {
        args.push("--chrome".to_string());
    }
    args.extend(req.extra_args.iter().cloned());
    args
}

/// stream-json の 1 イベントから session_id を拾う (init / result 等が持つ)。
pub fn extract_session_id(v: &Value) -> Option<String> {
    v.get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 進行中の claude セッション。
pub struct Session {
    child: Child,
    pub last_session_id: Arc<Mutex<Option<String>>>,
}

impl Session {
    /// まだ走っているか (`try_wait` が None)。
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// claude を kill する (best-effort)。
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        persist_session_id(&self.last_session_id.lock().unwrap().clone());
    }
}

/// claude を spawn し、stdout / stderr / exit を writer へ中継するスレッドを立てる。
pub fn spawn_claude<W: Write + Send + 'static>(
    claude: &Path,
    req: &StartRequest,
    writer: &Arc<SharedWriter<W>>,
    log: &Arc<DebugLog>,
) -> Result<Session, String> {
    let mut cmd = command_for(claude);
    cmd.args(build_claude_args(req))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("claude spawn 失敗 ({}): {e}", claude.display()))?;

    let last_session_id = Arc::new(Mutex::new(None::<String>));
    let stdin = child.stdin.take().ok_or("stdin pipe が取れない")?;
    let stdout = child.stdout.take().ok_or("stdout pipe が取れない")?;
    let stderr = child.stderr.take().ok_or("stderr pipe が取れない")?;

    // prompt を stdin に書いて閉じる (EOF = プロンプト確定)。pipe buffer が詰まっても
    // メインループを塞がないよう専用スレッドで書く。
    {
        let prompt = req.prompt.clone();
        let l = Arc::clone(log);
        std::thread::spawn(move || {
            let mut stdin = stdin;
            if let Err(e) = stdin.write_all(prompt.as_bytes()) {
                l.note("stdin_write_error", &e.to_string());
            }
            // drop で close → claude が読み終わる
        });
    }

    // stdout: JSONL → {type:"claude", data} (parse 不能行は {type:"raw"})。
    {
        let w = Arc::clone(writer);
        let l = Arc::clone(log);
        let sid = Arc::clone(&last_session_id);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let msg = match serde_json::from_str::<Value>(&line) {
                    Ok(data) => {
                        if let Some(s) = extract_session_id(&data) {
                            *sid.lock().unwrap() = Some(s);
                        }
                        json!({ "type": "claude", "data": data })
                    }
                    Err(_) => json!({ "type": "raw", "data": line }),
                };
                l.log("out", msg["type"].as_str().unwrap_or("?"), &msg);
                if w.send(&msg).is_err() {
                    break; // Chrome 側が閉じた
                }
            }
        });
    }

    // stderr: そのまま {type:"stderr"}。
    {
        let w = Arc::clone(writer);
        let l = Arc::clone(log);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let msg = json!({ "type": "stderr", "data": line });
                l.log("out", "stderr", &msg);
                if w.send(&msg).is_err() {
                    break;
                }
            }
        });
    }

    Ok(Session {
        child,
        last_session_id,
    })
}

/// exit を監視して通知する。メインループから毎コマンド時に呼ぶ軽量ポーリングではなく、
/// wait 専用スレッドを避けるため「stop / EOF / 次コマンド受信時」に確定させる方式を採る。
/// ここでは終了確認と通知だけ行う。
pub fn reap_if_exited<W: Write + Send + 'static>(
    session: &mut Session,
    writer: &Arc<SharedWriter<W>>,
    log: &Arc<DebugLog>,
) -> bool {
    match session.child.try_wait() {
        Ok(Some(status)) => {
            let sid = session.last_session_id.lock().unwrap().clone();
            persist_session_id(&sid);
            let msg = json!({
                "type": "proc",
                "event": "exit",
                "code": status.code(),
                "session_id": sid,
            });
            log.log("out", "proc", &msg);
            let _ = writer.send(&msg);
            true
        }
        _ => false,
    }
}

/// 直近 session_id を state ファイルに永続化する (`--resume` 用)。best-effort。
fn persist_session_id(sid: &Option<String>) {
    let Some(sid) = sid else { return };
    let Ok(dir) = register::data_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("last_session.json"),
        json!({ "session_id": sid }).to_string(),
    );
}

/// `.cmd` / `.bat` (npm shim) は直接 spawn できないので cmd /C 経由にする。
fn command_for(claude: &Path) -> Command {
    let ext = claude
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "cmd" || ext == "bat" {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(claude);
        c
    } else {
        Command::new(claude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_req(prompt: &str) -> StartRequest {
        StartRequest {
            prompt: prompt.to_string(),
            chrome: false,
            extra_args: vec![],
            cwd: None,
        }
    }

    #[test]
    fn parse_command_variants() {
        assert_eq!(parse_command(&json!({ "cmd": "ping" })), HostCommand::Ping);
        assert_eq!(parse_command(&json!({ "cmd": "stop" })), HostCommand::Stop);
        assert_eq!(
            parse_command(&json!({ "cmd": "check_update" })),
            HostCommand::CheckUpdate
        );
        assert_eq!(
            parse_command(&json!({ "cmd": "explode" })),
            HostCommand::Unknown("explode".to_string())
        );
        assert_eq!(
            parse_command(&json!({})),
            HostCommand::Unknown("".to_string())
        );
    }

    #[test]
    fn parse_start_full() {
        let v = json!({
            "cmd": "start",
            "prompt": "PR をレビューして",
            "chrome": true,
            "extra_args": ["--allowedTools", "Read"],
            "cwd": "C:\\work\\repo",
        });
        let HostCommand::Start(req) = parse_command(&v) else {
            panic!("Start になるはず");
        };
        assert_eq!(req.prompt, "PR をレビューして");
        assert!(req.chrome);
        assert_eq!(req.extra_args, vec!["--allowedTools", "Read"]);
        assert_eq!(req.cwd.as_deref(), Some("C:\\work\\repo"));
    }

    #[test]
    fn parse_start_defaults() {
        let HostCommand::Start(req) = parse_command(&json!({ "cmd": "start", "prompt": "x" }))
        else {
            panic!("Start になるはず");
        };
        assert!(!req.chrome);
        assert!(req.extra_args.is_empty());
        assert!(req.cwd.is_none());
    }

    #[test]
    fn build_args_baseline() {
        // prompt は argv に**入らない** (stdin 渡し。改行 / `-` 始まり行の mangle 防止)。
        let args = build_claude_args(&start_req("hello\n- [ ] --chrome を試す"));
        assert_eq!(
            args,
            vec!["-p", "--output-format", "stream-json", "--verbose"]
        );
    }

    #[test]
    fn build_args_with_chrome_and_extra() {
        let mut req = start_req("hello");
        req.chrome = true;
        req.extra_args = vec!["--allowedTools".into(), "Read".into()];
        let args = build_claude_args(&req);
        assert!(args.contains(&"--chrome".to_string()));
        assert_eq!(&args[args.len() - 2..], ["--allowedTools", "Read"]);
    }

    #[test]
    fn extracts_session_id() {
        let v = json!({ "type": "system", "subtype": "init", "session_id": "abc-123" });
        assert_eq!(extract_session_id(&v), Some("abc-123".to_string()));
        assert_eq!(extract_session_id(&json!({ "type": "assistant" })), None);
    }

    /// spawn → stdout 中継 → exit 通知の縦切りを、claude の代わりに /bin/sh で確認する
    /// (CCoW / Linux で走る実プロセステスト)。
    #[cfg(unix)]
    #[test]
    fn spawn_relays_stdout_and_exit() {
        use crate::nmhost::read_message;
        use std::io::Cursor;

        // 共有バッファへ書く writer。
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

        // 複数行 + `-` 始まり行の prompt が stdin 経由で**そのまま**届くことを固定する
        // (argv 渡し時代は cmd /C 分断 + unknown option で壊れた、#4 実機観測)。
        let req = StartRequest {
            prompt: "line1\n- [ ] --chrome を試す".to_string(),
            chrome: false,
            extra_args: vec![],
            cwd: None,
        };
        // "claude" の代役: stdin (= prompt) を読み切り、JSONL 1 行 + prompt をそのまま
        // echo して終了する sh (prompt round-trip の検証)。
        let dir = std::env::temp_dir().join(format!("ccwr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-claude");
        std::fs::write(
            &script,
            "#!/bin/sh\np=$(cat)\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-1\"}'\necho \"$p\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = Arc::new(DebugLog::disabled());
        let mut session = spawn_claude(&script, &req, &writer, &log).unwrap();
        // 子プロセスの出力スレッドが流し終わるのを待つ。
        let _ = session.child.wait();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(reap_if_exited(&mut session, &writer, &log));

        let bytes = buf.0.lock().unwrap().clone();
        let mut cursor = Cursor::new(bytes);
        let mut msgs = Vec::new();
        while let Some(m) = read_message(&mut cursor).unwrap() {
            msgs.push(m);
        }
        // claude(JSON) / raw(prompt 1 行目) / raw(prompt 2 行目) / proc(exit) の 4 件。
        assert_eq!(msgs[0]["type"], "claude");
        assert_eq!(msgs[0]["data"]["session_id"], "sid-1");
        assert_eq!(msgs[1]["type"], "raw");
        assert_eq!(msgs[1]["data"], "line1");
        assert_eq!(msgs[2]["type"], "raw");
        assert_eq!(msgs[2]["data"], "- [ ] --chrome を試す");
        assert_eq!(msgs[3]["type"], "proc");
        assert_eq!(msgs[3]["event"], "exit");
        assert_eq!(msgs[3]["code"], 0);
        assert_eq!(msgs[3]["session_id"], "sid-1");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

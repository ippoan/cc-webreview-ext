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

/// `{cmd:"start"}` / `{cmd:"resume"}` の入力。拡張から受けた JSON を検証済みの形に落とす。
#[derive(Debug, PartialEq, Default)]
pub struct StartRequest {
    pub prompt: String,
    pub chrome: bool,
    pub extra_args: Vec<String>,
    pub cwd: Option<String>,
    /// `--allowedTools` に comma join で組む permission rule 群 (#5 レビューフロー)。
    /// rule は `Bash(gh pr view:*)` のように空白を含むため extra_args (空白 split) では
    /// 運べず、独立フィールドで受けて argv 1 要素に組む。
    pub allowed_tools: Vec<String>,
    /// `--resume <sid>` (#5 失敗時の導線)。拡張からは渡せない — `{cmd:"resume"}` 受信時に
    /// host が last_session.json から読んで埋める。
    pub resume_session_id: Option<String>,
    /// レビュー対象 PR の key ("repo#number")。session_id と紐付けて state に永続化し、
    /// per-PR resume (docs/plan-review-flow.md 指摘3 の後続 (a)) を可能にする。
    /// レビュー以外の起動では None のままで良い。
    pub pr_key: Option<String>,
}

/// 拡張 → host の制御メッセージ。
#[derive(Debug, PartialEq)]
pub enum HostCommand {
    Ping,
    Start(StartRequest),
    /// 直近セッションの `--resume` 再実行 (#5)。session_id は host 側で解決する。
    Resume(StartRequest),
    /// レビュープロンプトの差し込み要求 (#5)。{type:"review_prompt"} で返す。
    ReviewPrompt(crate::review::PrInfo),
    Stop,
    /// 手動更新チェック (#6)。結果は {type:"update_status"} で返す。
    CheckUpdate,
    /// terminal session (#18): 対話モード claude を PTY 配下で開始。
    TermStart(crate::term::TermStart),
    /// xterm.js onData の入力文字列を PTY に書く。
    TermInput(String),
    /// 端末サイズ変更。
    TermResize {
        cols: u16,
        rows: u16,
    },
    /// terminal session を kill する。
    TermKill,
    /// debug.sqlite の直近 N 件を返す (--debug-dump の side panel 版、#18 診断導線)。
    DebugDump(i64),
    /// Claude in Chrome 連携の前提条件診断 (#31)。{type:"diag"} で返す。
    Diag,
    Unknown(String),
}

/// 制御メッセージの parse (純関数)。
pub fn parse_command(v: &Value) -> HostCommand {
    match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
        "ping" => HostCommand::Ping,
        "stop" => HostCommand::Stop,
        "check_update" => HostCommand::CheckUpdate,
        "term_start" => HostCommand::TermStart(crate::term::parse_term_start(v)),
        "term_input" => HostCommand::TermInput(
            v.get("data")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        "term_resize" => {
            let dim = |k: &str, default: u16| {
                v.get(k)
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(default)
            };
            HostCommand::TermResize {
                cols: dim("cols", 80),
                rows: dim("rows", 24),
            }
        }
        "term_kill" => HostCommand::TermKill,
        "debug_dump" => HostCommand::DebugDump(
            v.get("limit")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0 && *n <= 1000)
                .unwrap_or(50),
        ),
        "start" => HostCommand::Start(parse_start_request(v)),
        "resume" => HostCommand::Resume(parse_start_request(v)),
        "review_prompt" => HostCommand::ReviewPrompt(crate::review::parse_pr_info(v)),
        "diag" => HostCommand::Diag,
        other => HostCommand::Unknown(other.to_string()),
    }
}

/// `start` / `resume` 共通のフィールドを parse する。`resume_session_id` は
/// 拡張から受け取らない (host が state ファイルから解決する)。
fn parse_start_request(v: &Value) -> StartRequest {
    let str_array = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    StartRequest {
        prompt: v
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        chrome: v.get("chrome").and_then(Value::as_bool).unwrap_or(false),
        extra_args: str_array("extra_args"),
        cwd: v
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        allowed_tools: str_array("allowed_tools"),
        resume_session_id: None,
        pr_key: v
            .get("pr_key")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
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
    if let Some(sid) = &req.resume_session_id {
        args.push("--resume".to_string());
        args.push(sid.clone());
    }
    if !req.allowed_tools.is_empty() {
        // rule は空白を含む (`Bash(gh pr view:*)`) ため comma join して argv 1 要素で渡す。
        args.push("--allowedTools".to_string());
        args.push(req.allowed_tools.join(","));
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
    /// per-PR resume 用 (state 永続化時に session_id と紐付ける)。
    pr_key: Option<String>,
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
        persist_session_id(
            &self.last_session_id.lock().unwrap().clone(),
            self.pr_key.as_deref(),
        );
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
        pr_key: req.pr_key.clone(),
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
            persist_session_id(&sid, session.pr_key.as_deref());
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

/// state ファイルから resume 対象の session_id を読む (`{cmd:"resume"}` 用、#5)。
/// `pr_key` があれば per-PR map (`sessions`) を優先し、無ければグローバル直近
/// (`session_id`) にフォールバックする (per-PR map 化 = plan 指摘3 の後続 (a))。
pub fn load_session_id_for(pr_key: Option<&str>) -> Option<String> {
    let dir = register::data_dir().ok()?;
    let s = std::fs::read_to_string(dir.join("last_session.json")).ok()?;
    session_id_from_state_json(&s, pr_key)
}

/// last_session.json の中身から session_id を取り出す (純関数)。
/// 形式: `{"session_id": "<直近>", "sessions": {"repo#n": "<sid>", ...}}`。
/// 旧形式 (`session_id` のみ) もそのまま読める。
pub fn session_id_from_state_json(s: &str, pr_key: Option<&str>) -> Option<String> {
    let v = serde_json::from_str::<Value>(s).ok()?;
    let pick = |x: Option<&Value>| {
        x.and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    if let Some(key) = pr_key {
        if let Some(sid) = pick(v.get("sessions").and_then(|m| m.get(key))) {
            return Some(sid);
        }
    }
    pick(v.get("session_id"))
}

/// 既存 state に (sid, pr_key) を merge した新 state JSON を作る (純関数)。
/// `session_id` (グローバル直近) は常に更新、`pr_key` があれば `sessions` map にも書く。
/// 既存が parse できない場合は捨てて作り直す (state は resume 補助であり正でない)。
pub fn merge_session_state(existing: &str, sid: &str, pr_key: Option<&str>) -> String {
    let mut root = serde_json::from_str::<Value>(existing)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let obj = root.as_object_mut().expect("object を保証済み");
    obj.insert("session_id".to_string(), json!(sid));
    if let Some(key) = pr_key {
        let sessions = obj
            .entry("sessions".to_string())
            .or_insert_with(|| json!({}));
        if !sessions.is_object() {
            *sessions = json!({});
        }
        sessions
            .as_object_mut()
            .expect("object にした直後")
            .insert(key.to_string(), json!(sid));
    }
    root.to_string()
}

/// 直近 session_id を state ファイルに永続化する (`--resume` 用)。best-effort。
fn persist_session_id(sid: &Option<String>, pr_key: Option<&str>) {
    let Some(sid) = sid else { return };
    let Ok(dir) = register::data_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("last_session.json");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::write(path, merge_session_state(&existing, sid, pr_key));
}

/// `.cmd` / `.bat` (npm shim) は直接 spawn できないので cmd /C 経由にする。
pub(crate) fn command_for(claude: &Path) -> Command {
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
            ..StartRequest::default()
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
            parse_command(&json!({ "cmd": "term_input", "data": "ls\r" })),
            HostCommand::TermInput("ls\r".to_string())
        );
        assert_eq!(
            parse_command(&json!({ "cmd": "term_resize", "cols": 120, "rows": 30 })),
            HostCommand::TermResize {
                cols: 120,
                rows: 30
            }
        );
        assert_eq!(
            parse_command(&json!({ "cmd": "term_kill" })),
            HostCommand::TermKill
        );
        assert_eq!(
            parse_command(&json!({ "cmd": "debug_dump" })),
            HostCommand::DebugDump(50)
        );
        assert_eq!(
            parse_command(&json!({ "cmd": "debug_dump", "limit": 200 })),
            HostCommand::DebugDump(200)
        );
        // 範囲外は default に落とす
        assert_eq!(
            parse_command(&json!({ "cmd": "debug_dump", "limit": 999999 })),
            HostCommand::DebugDump(50)
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
    fn parse_start_with_allowed_tools() {
        let v = json!({
            "cmd": "start",
            "prompt": "review",
            "allowed_tools": ["Bash(gh pr view:*)", "Read"],
        });
        let HostCommand::Start(req) = parse_command(&v) else {
            panic!("Start になるはず");
        };
        assert_eq!(req.allowed_tools, vec!["Bash(gh pr view:*)", "Read"]);
        // resume_session_id は拡張から注入できない (host 側でのみ埋まる)
        assert!(req.resume_session_id.is_none());
    }

    #[test]
    fn parse_resume() {
        let v = json!({
            "cmd": "resume",
            "prompt": "続きから",
            "allowed_tools": ["Read"],
        });
        let HostCommand::Resume(req) = parse_command(&v) else {
            panic!("Resume になるはず");
        };
        assert_eq!(req.prompt, "続きから");
        assert_eq!(req.allowed_tools, vec!["Read"]);
        assert!(req.resume_session_id.is_none());
    }

    #[test]
    fn parse_diag_cmd() {
        assert_eq!(parse_command(&json!({ "cmd": "diag" })), HostCommand::Diag);
    }

    #[test]
    fn parse_review_prompt_cmd() {
        let v = json!({
            "cmd": "review_prompt",
            "pr": { "repo": "o/r", "number": 7, "url": "https://github.com/o/r/pull/7" },
        });
        let HostCommand::ReviewPrompt(pr) = parse_command(&v) else {
            panic!("ReviewPrompt になるはず");
        };
        assert_eq!(pr.repo, "o/r");
        assert_eq!(pr.number, 7);
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
    fn build_args_with_allowed_tools_joins_into_one_argv() {
        // rule は空白を含むため comma join した 1 引数で渡す (空白 split に載せない)。
        let mut req = start_req("review");
        req.allowed_tools = vec!["Bash(gh pr view:*)".into(), "Read".into()];
        let args = build_claude_args(&req);
        let i = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[i + 1], "Bash(gh pr view:*),Read");
    }

    #[test]
    fn build_args_with_resume() {
        let mut req = start_req("続きから");
        req.resume_session_id = Some("sid-9".into());
        let args = build_claude_args(&req);
        let i = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(args[i + 1], "sid-9");
    }

    #[test]
    fn session_id_from_state_json_variants() {
        // 旧形式 (session_id のみ) も読める
        assert_eq!(
            session_id_from_state_json(r#"{"session_id":"abc-1"}"#, None),
            Some("abc-1".to_string())
        );
        assert_eq!(
            session_id_from_state_json(r#"{"session_id":""}"#, None),
            None
        );
        assert_eq!(session_id_from_state_json("{}", None), None);
        assert_eq!(session_id_from_state_json("壊れてる", None), None);
    }

    #[test]
    fn per_pr_session_lookup_and_fallback() {
        let state = r#"{"session_id":"global-1","sessions":{"o/r#7":"sid-7","o/r#8":"sid-8"}}"#;
        // pr_key があれば per-PR を優先
        assert_eq!(
            session_id_from_state_json(state, Some("o/r#7")),
            Some("sid-7".to_string())
        );
        // 未知の pr_key はグローバル直近にフォールバック
        assert_eq!(
            session_id_from_state_json(state, Some("o/r#99")),
            Some("global-1".to_string())
        );
        // pr_key 無しはグローバル直近
        assert_eq!(
            session_id_from_state_json(state, None),
            Some("global-1".to_string())
        );
    }

    #[test]
    fn merge_session_state_builds_map() {
        // 空 → 新規作成
        let s1 = merge_session_state("", "sid-1", Some("o/r#7"));
        assert_eq!(
            session_id_from_state_json(&s1, Some("o/r#7")),
            Some("sid-1".to_string())
        );
        // 別 PR を追記しても既存 entry は残り、グローバル直近は更新される
        let s2 = merge_session_state(&s1, "sid-2", Some("o/r#8"));
        assert_eq!(
            session_id_from_state_json(&s2, Some("o/r#7")),
            Some("sid-1".to_string())
        );
        assert_eq!(
            session_id_from_state_json(&s2, Some("o/r#8")),
            Some("sid-2".to_string())
        );
        assert_eq!(
            session_id_from_state_json(&s2, None),
            Some("sid-2".to_string())
        );
        // pr_key 無しの起動はグローバル直近だけ更新
        let s3 = merge_session_state(&s2, "sid-3", None);
        assert_eq!(
            session_id_from_state_json(&s3, Some("o/r#8")),
            Some("sid-2".to_string())
        );
        assert_eq!(
            session_id_from_state_json(&s3, None),
            Some("sid-3".to_string())
        );
        // 壊れた既存 state は捨てて作り直す
        let s4 = merge_session_state("[1,2]", "sid-4", Some("o/r#9"));
        assert_eq!(
            session_id_from_state_json(&s4, Some("o/r#9")),
            Some("sid-4".to_string())
        );
    }

    #[test]
    fn parse_start_with_pr_key() {
        let v = json!({ "cmd": "start", "prompt": "x", "pr_key": "o/r#7" });
        let HostCommand::Start(req) = parse_command(&v) else {
            panic!("Start になるはず");
        };
        assert_eq!(req.pr_key.as_deref(), Some("o/r#7"));
        // 空文字は None に落とす
        let v = json!({ "cmd": "start", "prompt": "x", "pr_key": "" });
        let HostCommand::Start(req) = parse_command(&v) else {
            panic!("Start になるはず");
        };
        assert!(req.pr_key.is_none());
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
            ..StartRequest::default()
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

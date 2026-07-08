//! cc-webreview-agent — Chrome 拡張から claude を起動し stream-json を中継する
//! Native Messaging host (cc-webreview-ext#3)。
//!
//! 起動モード:
//! - Chrome からの native host 起動 (argv に `chrome-extension://…` origin が来る) または
//!   `--native-host` → stdio ループ
//! - `--register [--claude-path <abs>] [--extension-id <id>]` → HKCU 登録 (Windows)
//! - `--print-manifest` → native host manifest を stdout に出す (デバッグ用)
//! - `--debug-dump [N]` → debug.sqlite の直近 N 件 (default 100) を JSONL で出す
//! - それ以外 → usage (stdin 待ちで固まらないように)

mod debuglog;
mod nmhost;
mod register;
mod session;

use debuglog::DebugLog;
use nmhost::SharedWriter;
use serde_json::json;
use session::{HostCommand, Session};
use std::io;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--register") {
        run_register(&args);
    } else if args.iter().any(|a| a == "--print-manifest") {
        let exe = std::env::current_exe().unwrap_or_default();
        println!(
            "{}",
            register::native_host_manifest_json(&exe, register::EXT_ID)
        );
    } else if args.iter().any(|a| a == "--debug-dump") {
        let limit = flag_value(&args, "--debug-dump")
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        match DebugLog::open_default().dump(limit) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
            }
            Err(e) => {
                eprintln!("debug-dump 失敗: {e}");
                std::process::exit(1);
            }
        }
    } else if is_native_host_invocation(&args) {
        run_native_host();
    } else {
        eprintln!(
            "usage: cc-webreview-agent --register [--claude-path <abs path>] [--extension-id <id>]"
        );
        eprintln!("       cc-webreview-agent --print-manifest");
        eprintln!("       cc-webreview-agent --debug-dump [N]");
        eprintln!("       (Chrome からは native messaging 経由で起動される)");
        std::process::exit(2);
    }
}

/// argv が native-host 起動か判定する。Chrome は origin (`chrome-extension://…`) を渡す。
fn is_native_host_invocation(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--native-host" || a.starts_with("chrome-extension://"))
}

/// `--register` 処理。`--claude-path` は絶対パス必須。
fn run_register(args: &[String]) {
    let claude = match flag_value(args, "--claude-path") {
        Some(p) => match register::validate_claude_path(&p) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    let ext_id = flag_value(args, "--extension-id").unwrap_or_else(|| register::EXT_ID.to_string());
    match register::install(claude.as_deref(), &ext_id) {
        Ok(msg) => println!("{msg}"),
        Err(e) => {
            eprintln!("登録失敗: {e}");
            std::process::exit(1);
        }
    }
}

/// `--flag value` 形式の値を取る。
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// native-host の stdio ループ。Chrome が port を閉じる (EOF) まで読み続ける。
/// stdout は framed JSON 専用 (log は stderr)。
fn run_native_host() {
    let stdin = io::stdin();
    let mut r = stdin.lock();
    let writer = Arc::new(SharedWriter::new(io::stdout()));
    let log = Arc::new(DebugLog::open_default());
    let version = env!("CARGO_PKG_VERSION");

    // host → 拡張への送信は必ずここを通す (debug.sqlite に記録してから送る)。
    let emit = |v: serde_json::Value| {
        log.log("out", v["type"].as_str().unwrap_or("?"), &v);
        let _ = writer.send(&v);
    };

    let claude = register::resolve_claude_path();
    eprintln!(
        "[cc-webreview-agent] native-host mode (claude={})",
        claude
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未解決".to_string())
    );
    log.note("boot", &format!("v{version}"));
    emit(json!({
        "type": "hello",
        "version": version,
        "claude": claude.as_ref().map(|p| p.display().to_string()),
    }));

    let mut active: Option<Session> = None;

    loop {
        let req = match nmhost::read_message(&mut r) {
            Ok(Some(v)) => v,
            Ok(None) => break, // Chrome が port を閉じた
            Err(e) => {
                log.note("read_error", &e.to_string());
                emit(json!({ "type": "error", "error": e.to_string() }));
                break;
            }
        };
        log.log("in", req["cmd"].as_str().unwrap_or("?"), &req);

        // コマンド処理の前に、終了済みセッションを回収して exit 通知する。
        if let Some(s) = active.as_mut() {
            if session::reap_if_exited(s, &writer, &log) {
                active = None;
            }
        }

        match session::parse_command(&req) {
            HostCommand::Ping => {
                let claude = register::resolve_claude_path();
                emit(json!({
                    "type": "pong",
                    "version": version,
                    "claude": claude.as_ref().map(|p| p.display().to_string()),
                    "running": active.as_mut().map(Session::is_running).unwrap_or(false),
                }));
            }
            HostCommand::Start(start) => {
                if active.as_mut().map(Session::is_running).unwrap_or(false) {
                    emit(json!({ "type": "busy" }));
                    continue;
                }
                if start.prompt.trim().is_empty() {
                    emit(json!({ "type": "error", "error": "prompt が空" }));
                    continue;
                }
                let Some(claude) = register::resolve_claude_path() else {
                    emit(json!({
                        "type": "error",
                        "error": "claude が見つからない。--register --claude-path <abs> で設定してください",
                    }));
                    continue;
                };
                match session::spawn_claude(&claude, &start, &writer, &log) {
                    Ok(s) => {
                        emit(json!({ "type": "proc", "event": "spawn" }));
                        active = Some(s);
                    }
                    Err(e) => {
                        emit(json!({ "type": "error", "error": e }));
                    }
                }
            }
            HostCommand::Stop => {
                if let Some(mut s) = active.take() {
                    s.kill();
                    emit(json!({ "type": "proc", "event": "killed" }));
                } else {
                    emit(json!({ "type": "proc", "event": "not_running" }));
                }
            }
            HostCommand::Unknown(cmd) => {
                emit(json!({
                    "type": "error",
                    "error": format!("unknown cmd: {cmd}"),
                }));
            }
        }
    }

    // port 切断: ゾンビを残さない (session_id は kill 内で永続化される)。
    log.note("eof", "port closed");
    if let Some(mut s) = active.take() {
        eprintln!("[cc-webreview-agent] port 切断 → claude を kill");
        log.note("kill_on_eof", "claude killed");
        s.kill();
    }
}

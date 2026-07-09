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

mod auth;
mod debuglog;
mod nmhost;
mod register;
mod review;
mod session;
mod term;
mod trust;
mod update;

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

/// cwd 未指定の spawn を安定 work dir に固定し、その dir だけを claude に事前 trust
/// 登録する (#28 — Chrome 継承 cwd は起動経路で変わり trust プロンプトが毎回出る)。
/// ユーザーが明示指定した cwd は変更も trust 登録もしない。全て best-effort:
/// 失敗しても spawn は続行する (trust プロンプトが出るだけ)。
fn pin_default_cwd(cwd: &mut Option<String>, log: &debuglog::DebugLog) {
    if cwd.is_some() {
        return;
    }
    let Some(dir) = register::default_work_dir() else {
        return;
    };
    match trust::ensure_trusted(&dir) {
        Ok(true) => log.note("trust_registered", &dir.display().to_string()),
        Ok(false) => {}
        Err(e) => log.note("trust_error", &e),
    }
    *cwd = Some(dir.display().to_string());
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
    // release tag (agent-dev-N / agent-vX.Y.Z)。ローカルビルドでは None。
    let release_tag = option_env!("CC_WEBREVIEW_RELEASE_TAG");

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
        "release_tag": release_tag,
        "claude": claude.as_ref().map(|p| p.display().to_string()),
        // 認証状態 (boolean のみ、secret の値は含まない — #13)
        "auth": auth::AuthStatus::probe().to_json(),
    }));

    // 更新チェック (#6) はバックグラウンドで行い、stdio ループを塞がない。
    // agent 本体は self_replace され**次回起動で反映**、拡張は extension\ を上書きし
    // Chrome の拡張リロードで反映される。起動時は適用があった時のみ通知。
    spawn_update_check(&writer, &log, false);

    let mut active: Option<Session> = None;
    let mut active_term: Option<term::TermSession> = None;

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
        // terminal の exit は reader スレッドが {type:"term_exit"} を通知済みなので回収のみ。
        if let Some(t) = active_term.as_mut() {
            if !t.is_running() {
                active_term = None;
            }
        }
        // 「同時 claude 1 本」規約: -p / terminal どちらかが走っていれば busy。
        let busy = active.as_mut().map(Session::is_running).unwrap_or(false)
            || active_term
                .as_mut()
                .map(term::TermSession::is_running)
                .unwrap_or(false);

        match session::parse_command(&req) {
            HostCommand::Ping => {
                let claude = register::resolve_claude_path();
                emit(json!({
                    "type": "pong",
                    "version": version,
                    "release_tag": release_tag,
                    "claude": claude.as_ref().map(|p| p.display().to_string()),
                    "running": active.as_mut().map(Session::is_running).unwrap_or(false),
                    // 認証状態 (boolean のみ、secret の値は含まない — #13)
                    "auth": auth::AuthStatus::probe().to_json(),
                }));
            }
            HostCommand::Start(mut start) => {
                if busy {
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
                pin_default_cwd(&mut start.cwd, &log);
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
            HostCommand::Resume(mut start) => {
                // 直近 -p セッションの `--resume` 再実行 (#5 失敗時の導線)。
                // state はグローバル 1 本 = 直近 1 件のみ (UI 文言でも明示)。
                if busy {
                    emit(json!({ "type": "busy" }));
                    continue;
                }
                if start.prompt.trim().is_empty() {
                    emit(json!({ "type": "error", "error": "prompt が空" }));
                    continue;
                }
                let Some(sid) = session::load_last_session_id() else {
                    emit(json!({
                        "type": "error",
                        "error": "resume できるセッションが無い (直近の -p セッション記録が未作成)",
                    }));
                    continue;
                };
                // パネル再読み込みで拡張側の allowlist が消えていても resume を
                // 空振りさせない (レビュー既定の read-only allowlist を補う)。
                start.allowed_tools = review::allowlist_or_review_default(start.allowed_tools);
                let Some(claude) = register::resolve_claude_path() else {
                    emit(json!({
                        "type": "error",
                        "error": "claude が見つからない。--register --claude-path <abs> で設定してください",
                    }));
                    continue;
                };
                start.resume_session_id = Some(sid);
                pin_default_cwd(&mut start.cwd, &log);
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
            HostCommand::ReviewPrompt(pr) => {
                // テンプレ差し込み (#5)。read-only なので busy でも応答してよい。
                emit(json!({
                    "type": "review_prompt",
                    "prompt": review::render_review_prompt(&pr),
                    "allowed_tools": review::review_allowed_tools(),
                    "pr": { "repo": pr.repo, "number": pr.number, "url": pr.url },
                }));
            }
            HostCommand::CheckUpdate => {
                // 手動更新チェック (side panel の「更新確認」ボタン)。結果は
                // {type:"update_status"} で全件返す (最新でもフィードバックを出す)。
                spawn_update_check(&writer, &log, true);
            }
            HostCommand::TermStart(mut start) => {
                if busy {
                    emit(json!({ "type": "busy" }));
                    continue;
                }
                let Some(claude) = register::resolve_claude_path() else {
                    emit(json!({
                        "type": "error",
                        "error": "claude が見つからない。--register --claude-path <abs> で設定してください",
                    }));
                    continue;
                };
                pin_default_cwd(&mut start.cwd, &log);
                match term::spawn_terminal(&claude, &start, &writer, &log) {
                    Ok(t) => {
                        emit(json!({ "type": "proc", "event": "term_spawn" }));
                        active_term = Some(t);
                    }
                    Err(e) => {
                        emit(json!({ "type": "error", "error": e }));
                    }
                }
            }
            HostCommand::TermInput(data) => {
                if let Some(t) = active_term.as_mut() {
                    if let Err(e) = t.write_input(&data) {
                        emit(json!({ "type": "error", "error": e }));
                    }
                } else {
                    emit(json!({ "type": "error", "error": "terminal が起動していない" }));
                }
            }
            HostCommand::TermResize { cols, rows } => {
                if let Some(t) = active_term.as_ref() {
                    t.resize(cols, rows);
                }
            }
            HostCommand::DebugDump(limit) => {
                // side panel の「debug コピー」。dump 自体は debug.sqlite の read のみ。
                // 大きくても SharedWriter が chunk 分割する。
                match log.dump(limit) {
                    Ok(lines) => emit(json!({ "type": "debug_dump", "lines": lines })),
                    Err(e) => {
                        emit(json!({ "type": "error", "error": format!("debug dump 失敗: {e}") }))
                    }
                }
            }
            HostCommand::TermKill => {
                if let Some(mut t) = active_term.take() {
                    t.kill();
                    emit(json!({ "type": "proc", "event": "term_killed" }));
                } else {
                    emit(json!({ "type": "proc", "event": "not_running" }));
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
    if let Some(mut t) = active_term.take() {
        eprintln!("[cc-webreview-agent] port 切断 → terminal claude を kill");
        log.note("kill_on_eof", "terminal killed");
        t.kill();
    }
}

/// 更新チェック (#6) を別スレッドで実行する (stdio ループを塞がない)。
/// - 起動時 (`manual=false`): 適用があった時だけ `{type:"update"}` を通知
/// - 手動 (`manual=true`、`{cmd:"check_update"}`): 最新/対象外/失敗も含め全結果を
///   `{type:"update_status", component, status, tag?, error?}` で返す
fn spawn_update_check(writer: &Arc<SharedWriter<io::Stdout>>, log: &Arc<DebugLog>, manual: bool) {
    let writer = Arc::clone(writer);
    let log = Arc::clone(log);
    std::thread::spawn(move || {
        let send = |v: serde_json::Value| {
            log.log("out", v["type"].as_str().unwrap_or("?"), &v);
            let _ = writer.send(&v);
        };
        let status = |component: &str, st: &str, tag: Option<&str>, error: Option<&str>| {
            send(json!({
                "type": "update_status",
                "component": component,
                "status": st,
                "tag": tag,
                "error": error,
            }));
        };

        // agent 本体。ローカル dev ビルド (tag なし) は自動更新対象外。
        if update::current_release_tag().is_none() {
            if manual {
                status("agent", "dev_build", None, None);
            }
        } else {
            match update::check_and_self_update() {
                Ok(Some(tag)) => {
                    if manual {
                        status("agent", "applied", Some(&tag), None);
                    } else {
                        send(json!({ "type": "update", "component": "agent", "tag": tag }));
                    }
                }
                Ok(None) => {
                    if manual {
                        status("agent", "up_to_date", None, None);
                    }
                }
                Err(e) => {
                    eprintln!("[cc-webreview-agent] self-update: {e}");
                    log.note("self_update_error", &e);
                    if manual {
                        status("agent", "error", None, Some(&e));
                    }
                }
            }
        }

        // 同梱拡張。
        match update::update_extension() {
            Ok(Some(tag)) => {
                if manual {
                    status("extension", "applied", Some(&tag), None);
                } else {
                    send(json!({ "type": "update", "component": "extension", "tag": tag }));
                }
            }
            Ok(None) => {
                if manual {
                    status("extension", "up_to_date", None, None);
                }
            }
            Err(e) => {
                eprintln!("[cc-webreview-agent] extension update: {e}");
                log.note("ext_update_error", &e);
                if manual {
                    status("extension", "error", None, Some(&e));
                }
            }
        }
    });
}

//! SQLite debug ログ (cc-webreview-ext#3)。
//!
//! native host の stderr は Chrome に飲まれて事後調査できないため、host を通る全イベント
//! (拡張→host の制御メッセージ / host→拡張の中継メッセージ / stderr / proc) を
//! `%LOCALAPPDATA%\cc-webreview\debug.sqlite` に書き、後から
//! `cc-webreview-agent --debug-dump [N]` や sqlite3 CLI で取り出せるようにする。
//!
//! 方針:
//! - ログ失敗で host を絶対に落とさない (best-effort、エラーは stderr のみ)。
//! - payload は 64KB で truncate (無限肥大防止)。open 時に古い行を prune。

use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

/// 1 行の payload 上限 (超過分は truncate)。
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// 保持する最大行数 (open 時にこれを超える古い行を削除)。
const KEEP_ROWS: i64 = 20_000;

/// best-effort な SQLite ロガー。open に失敗したら以後 no-op。
pub struct DebugLog {
    conn: Option<Mutex<Connection>>,
}

impl DebugLog {
    /// 何も記録しない no-op ロガー (test 用)。
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self { conn: None }
    }

    /// data_dir 配下の debug.sqlite を開く (無ければ作る)。失敗時は no-op ロガーを返す。
    pub fn open_default() -> Self {
        let path = match crate::register::data_dir() {
            Ok(dir) => {
                let _ = std::fs::create_dir_all(&dir);
                dir.join("debug.sqlite")
            }
            Err(e) => {
                eprintln!("[cc-webreview-agent] debug log 無効 (data_dir 不明): {e}");
                return Self { conn: None };
            }
        };
        Self::open(&path)
    }

    /// 指定パスで開く (test 用にも公開)。
    pub fn open(path: &Path) -> Self {
        match Self::try_open(path) {
            Ok(conn) => Self {
                conn: Some(Mutex::new(conn)),
            },
            Err(e) => {
                eprintln!(
                    "[cc-webreview-agent] debug log 無効 ({}): {e}",
                    path.display()
                );
                Self { conn: None }
            }
        }
    }

    fn try_open(path: &Path) -> rusqlite::Result<Connection> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS events (
               id      INTEGER PRIMARY KEY AUTOINCREMENT,
               ts      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               pid     INTEGER NOT NULL,
               dir     TEXT NOT NULL,   -- 'in' | 'out' | 'host'
               kind    TEXT NOT NULL,   -- cmd 名 / message type
               payload TEXT NOT NULL
             );",
        )?;
        // 古い行の prune (best-effort)。
        conn.execute(
            "DELETE FROM events
             WHERE id <= (SELECT COALESCE(MAX(id),0) FROM events) - ?1",
            [KEEP_ROWS],
        )?;
        Ok(conn)
    }

    /// イベントを 1 行記録する。失敗は握り潰す (stderr にだけ出す)。
    pub fn log(&self, dir: &str, kind: &str, payload: &Value) {
        let Some(conn) = &self.conn else { return };
        let body = truncate_utf8(&payload.to_string(), MAX_PAYLOAD_BYTES);
        let r = conn.lock().unwrap().execute(
            "INSERT INTO events (pid, dir, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![std::process::id(), dir, kind, body],
        );
        if let Err(e) = r {
            eprintln!("[cc-webreview-agent] debug log insert 失敗: {e}");
        }
    }

    /// host 自身のライフサイクルイベント (起動 / EOF / kill 等) を記録する。
    pub fn note(&self, kind: &str, detail: &str) {
        self.log("host", kind, &Value::String(detail.to_string()));
    }

    /// 直近 `limit` 件を古い順の JSONL で返す (--debug-dump 用)。
    pub fn dump(&self, limit: i64) -> Result<Vec<String>, String> {
        let Some(conn) = &self.conn else {
            return Err("debug log が開けていない".to_string());
        };
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, ts, pid, dir, kind, payload FROM events
                 ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([limit], |row| {
                let (id, ts, pid, dir, kind, payload): (i64, String, i64, String, String, String) = (
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                );
                Ok(serde_json::json!({
                    "id": id, "ts": ts, "pid": pid, "dir": dir, "kind": kind,
                    // payload は JSON 文字列として保存しているので、可能なら埋め直す。
                    "payload": serde_json::from_str::<Value>(&payload)
                        .unwrap_or(Value::String(payload)),
                })
                .to_string())
            })
            .map_err(|e| e.to_string())?;
        let mut out: Vec<String> = rows.filter_map(Result::ok).collect();
        out.reverse(); // 古い順
        Ok(out)
    }
}

/// UTF-8 の char 境界を守って max_bytes に truncate する。
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated {} bytes)", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ccwr-dbg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("debug.sqlite")
    }

    #[test]
    fn logs_and_dumps_in_order() {
        let db = tmp_db("order");
        let log = DebugLog::open(&db);
        log.log("in", "start", &json!({ "cmd": "start", "prompt": "x" }));
        log.log(
            "out",
            "claude",
            &json!({ "type": "claude", "data": { "n": 1 } }),
        );
        log.note("eof", "port closed");

        let lines = log.dump(10).unwrap();
        assert_eq!(lines.len(), 3);
        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        let last: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(first["dir"], "in");
        assert_eq!(first["kind"], "start");
        assert_eq!(first["payload"]["prompt"], "x");
        assert_eq!(last["dir"], "host");
        assert_eq!(last["kind"], "eof");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn dump_respects_limit_returning_latest() {
        let db = tmp_db("limit");
        let log = DebugLog::open(&db);
        for i in 0..5 {
            log.log("out", "claude", &json!({ "i": i }));
        }
        let lines = log.dump(2).unwrap();
        assert_eq!(lines.len(), 2);
        let v: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(v["payload"]["i"], 4); // 最新が末尾 (古い順)
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn oversized_payload_is_truncated() {
        let db = tmp_db("trunc");
        let log = DebugLog::open(&db);
        let big = "あ".repeat(MAX_PAYLOAD_BYTES); // 3 bytes/char → 上限超過
        log.log("out", "claude", &json!({ "data": big }));
        let lines = log.dump(1).unwrap();
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        let payload = v["payload"].as_str().unwrap(); // JSON として壊れる → 文字列 fallback
        assert!(payload.contains("truncated"));
        assert!(payload.len() < MAX_PAYLOAD_BYTES + 100);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "あいうえお";
        let t = truncate_utf8(s, 4); // 「あ」=3bytes、4 は「い」の途中 → 3 に丸まる
        assert!(t.starts_with("あ"));
        assert!(!t.starts_with("あい"));
    }

    #[test]
    fn noop_logger_does_not_panic() {
        let log = DebugLog::disabled();
        log.log("in", "ping", &json!({}));
        assert!(log.dump(1).is_err());
    }
}

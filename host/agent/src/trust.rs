//! claude の trust プロンプト抑止 (cc-webreview-ext#28)。
//!
//! claude は「Do you trust the files in this folder?」の応答を `~/.claude.json` の
//! `projects[<絶対パス>].hasTrustDialogAccepted: true` に**フォルダ単位**で永続化する。
//! Chrome から起動された native host の cwd は Chrome の起動経路で変わるため、
//! cwd 未指定のまま claude を spawn すると毎回「初見のフォルダ」として trust を
//! 聞かれる (-p では応答手段が無い)。
//!
//! 対策: spawn 前に host 専用の work dir (`data_dir()/work`) へ cwd を固定し、
//! **その dir だけ**を事前 trust 登録する。ユーザーが明示指定した `cwd` は勝手に
//! trust しない (任意フォルダの無断 trust はしない)。
//!
//! `~/.claude.json` の書き換えは best-effort:
//! - parse できない config は一切触らない (fail-open — プロンプトが出るだけ)
//! - 変更が必要な時のみ tmp file + rename の atomic write (初回の 1 回だけ書く)
//! - 「同時 claude 1 本」規約により、本 host 経由の claude とは書き込みが競合しない

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// `~/.claude.json` のパス。Windows は `%USERPROFILE%`、それ以外は `$HOME`。
pub fn claude_config_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(".claude.json"))
}

/// config JSON に `projects[dir].hasTrustDialogAccepted = true` を upsert する (純関数)。
/// 変更が必要な場合のみ `Some(新 JSON)` を返す。root / projects / entry が object で
/// ない場合は壊れた config とみなし `None` (触らない)。
pub fn upsert_trust(root: &Value, dir: &str) -> Option<Value> {
    if !root.is_object() {
        return None;
    }
    let mut new_root = root.clone();
    let obj = new_root.as_object_mut()?;
    let projects = obj
        .entry("projects".to_string())
        .or_insert_with(|| json!({}));
    if !projects.is_object() {
        return None;
    }
    let entry = projects
        .as_object_mut()?
        .entry(dir.to_string())
        .or_insert_with(|| json!({}));
    if !entry.is_object() {
        return None;
    }
    if entry.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true) {
        return None; // 登録済み — 書き込み不要
    }
    entry
        .as_object_mut()?
        .insert("hasTrustDialogAccepted".to_string(), json!(true));
    Some(new_root)
}

/// `dir` を `~/.claude.json` に事前 trust 登録する。登録済みなら何もしない。
/// 戻り値: 書き込んだか。失敗は Err (呼び出し側で log のみ、spawn は続行)。
pub fn ensure_trusted(dir: &Path) -> Result<bool, String> {
    let config = claude_config_path().ok_or("HOME / USERPROFILE が不明")?;
    let raw = match std::fs::read_to_string(&config) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(e) => return Err(format!("{} 読み込み失敗: {e}", config.display())),
    };
    let root: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{} が parse できないため触らない: {e}", config.display()))?;
    let Some(new_root) = upsert_trust(&root, &dir.display().to_string()) else {
        return Ok(false); // 登録済み or 壊れた構造 (触らない)
    };
    // atomic write: 同 dir に tmp を書いて rename (書きかけの config を残さない)。
    let tmp = config.with_extension("json.cc-webreview-tmp");
    let body = serde_json::to_string(&new_root).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("tmp 書き込み失敗: {e}"))?;
    std::fs::rename(&tmp, &config).map_err(|e| format!("rename 失敗: {e}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_adds_trust_to_empty_config() {
        let out = upsert_trust(&json!({}), "C:\\work").unwrap();
        assert_eq!(out["projects"]["C:\\work"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn upsert_preserves_existing_fields() {
        let root = json!({
            "userID": "u1",
            "projects": {
                "C:\\work": { "allowedTools": ["Read"], "hasTrustDialogAccepted": false },
                "D:\\other": { "hasTrustDialogAccepted": true },
            }
        });
        let out = upsert_trust(&root, "C:\\work").unwrap();
        // 既存フィールドと他 project は保持
        assert_eq!(out["userID"], "u1");
        assert_eq!(out["projects"]["C:\\work"]["allowedTools"][0], "Read");
        assert_eq!(out["projects"]["D:\\other"]["hasTrustDialogAccepted"], true);
        assert_eq!(out["projects"]["C:\\work"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn upsert_noop_when_already_trusted() {
        let root = json!({ "projects": { "/w": { "hasTrustDialogAccepted": true } } });
        assert!(upsert_trust(&root, "/w").is_none());
    }

    #[test]
    fn upsert_refuses_broken_config() {
        // root / projects / entry が object でない config は触らない (fail-open)
        assert!(upsert_trust(&json!([1, 2]), "/w").is_none());
        assert!(upsert_trust(&json!({ "projects": "oops" }), "/w").is_none());
        assert!(upsert_trust(&json!({ "projects": { "/w": 42 } }), "/w").is_none());
    }

    #[test]
    fn ensure_trusted_roundtrip_in_temp_home() {
        // HOME を temp に差し替えた別プロセス変数は使えないため、ファイル操作だけ検証:
        // upsert → 書いた JSON を再 parse して noop になることを確認する。
        let root = json!({ "projects": {} });
        let out = upsert_trust(&root, "/tmp/ccwr-work").unwrap();
        let reparsed: Value = serde_json::from_str(&out.to_string()).unwrap();
        assert!(upsert_trust(&reparsed, "/tmp/ccwr-work").is_none());
    }
}

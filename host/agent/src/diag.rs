//! Claude in Chrome 接続まわりの環境診断 (#31)。
//!
//! side panel の「診断」ボタン → `{cmd:"diag"}` に対し、headless レビューで
//! browser ツールを使う前提条件 (claude のパスと解決経路・`--version`・認証 token の
//! 有無) を集めて `{type:"diag"}` で返す。拡張側の残り (公式 Claude in Chrome 拡張の
//! インストール有無) は side panel が `chrome.management` で確認する。
//! auth.rs と同じ方針で **secret の値は一切含めない** (boolean と版数のみ)。

use serde_json::{json, Value};

/// Claude in Chrome 連携に必要な claude の最低版 (公式 docs: 2.0.73+)。
pub const MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 0, 73);

/// `claude --version` の出力から `x.y.z` を取り出す (例 "2.0.76 (Claude Code)")。
/// 先頭の `v` は許容する。3 要素に満たない数字列は無視する。
pub fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    for tok in output.split_whitespace() {
        let tok = tok.strip_prefix('v').unwrap_or(tok);
        let mut parts = tok.split('.');
        let (Some(a), Some(b), Some(c)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // patch 部は "76-beta" のような suffix を許容 (数字 prefix のみ読む)
        let digits = |s: &str| -> Option<u64> {
            let d: String = s.chars().take_while(char::is_ascii_digit).collect();
            if d.is_empty() {
                None
            } else {
                d.parse().ok()
            }
        };
        if let (Some(a), Some(b), Some(c)) = (digits(a), digits(b), digits(c)) {
            return Some((a, b, c));
        }
    }
    None
}

/// version 比較 (tuple の辞書順 = semver の数値比較)。
pub fn version_at_least(v: (u64, u64, u64), min: (u64, u64, u64)) -> bool {
    v >= min
}

/// `{type:"diag"}` 応答を組む純関数 (テスト注入点)。
/// `claude_path` / `claude_source` は resolve 結果、`version_output` は
/// `claude --version` の stdout。auth は boolean のみ (値を含まない)。
pub fn diag_json(
    claude_path: Option<&str>,
    claude_source: Option<&str>,
    version_output: Option<&str>,
    auth: crate::auth::AuthStatus,
) -> Value {
    let parsed = version_output.and_then(parse_version);
    json!({
        "type": "diag",
        "claude_path": claude_path,
        "claude_source": claude_source,
        "claude_version": parsed.map(|(a, b, c)| format!("{a}.{b}.{c}")),
        "claude_version_ok": parsed.map(|v| version_at_least(v, MIN_CLAUDE_VERSION)),
        "min_claude_version": format!(
            "{}.{}.{}",
            MIN_CLAUDE_VERSION.0, MIN_CLAUDE_VERSION.1, MIN_CLAUDE_VERSION.2
        ),
        "auth": auth.to_json(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStatus;
    use std::ffi::OsString;

    #[test]
    fn parses_version_from_typical_outputs() {
        assert_eq!(parse_version("2.0.76 (Claude Code)"), Some((2, 0, 76)));
        assert_eq!(parse_version("claude v2.0.73"), Some((2, 0, 73)));
        assert_eq!(parse_version("2.1.0-beta.1 something"), Some((2, 1, 0)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("2.0"), None); // 3 要素未満は不採用
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_gate_boundaries() {
        assert!(!version_at_least((2, 0, 72), MIN_CLAUDE_VERSION));
        assert!(version_at_least((2, 0, 73), MIN_CLAUDE_VERSION));
        assert!(version_at_least((2, 1, 0), MIN_CLAUDE_VERSION));
        assert!(version_at_least((3, 0, 0), MIN_CLAUDE_VERSION));
        assert!(!version_at_least((1, 9, 99), MIN_CLAUDE_VERSION));
    }

    #[test]
    fn diag_json_shape_and_no_secret() {
        // token の値を注入しても diag 出力には一切現れない (auth.rs #13 と同方針)
        let auth = AuthStatus::from_parts(false, Some(OsString::from("sk-ant-oat01-DUMMY")));
        let v = diag_json(
            Some("C:\\Users\\me\\.local\\bin\\claude.exe"),
            Some("registry ClaudeExe"),
            Some("2.0.76 (Claude Code)"),
            auth,
        );
        assert_eq!(v["type"], "diag");
        assert_eq!(v["claude_version"], "2.0.76");
        assert_eq!(v["claude_version_ok"], true);
        assert_eq!(v["min_claude_version"], "2.0.73");
        assert_eq!(v["auth"]["oauth_token_env"], true);
        assert!(!v.to_string().contains("DUMMY"));
    }

    #[test]
    fn diag_json_handles_missing_claude() {
        let v = diag_json(None, None, None, AuthStatus::from_parts(false, None));
        assert_eq!(v["claude_path"], Value::Null);
        assert_eq!(v["claude_version"], Value::Null);
        assert_eq!(v["claude_version_ok"], Value::Null);
        assert_eq!(v["auth"]["likely_logged_in"], false);
    }

    #[test]
    fn diag_json_flags_old_version() {
        let v = diag_json(
            Some("/x/claude"),
            Some("env CC_WEBREVIEW_CLAUDE"),
            Some("2.0.60"),
            AuthStatus::from_parts(true, None),
        );
        assert_eq!(v["claude_version_ok"], false);
    }
}

//! 認証状態の軽量判定 (cc-webreview-ext#13)。
//!
//! side panel の login 導線用に「ログインできていそうか」を boolean だけで返す。
//! **secret の値は一切読まない・保持しない・出力しない** — credentials ファイルは
//! 存在確認のみ (open しない)、env token は「非空か」を即 bool 化する。
//! JSON 化した結果に boolean 以外が混ざらないことは test で固定する。

use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 認証状態 (boolean のみ)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuthStatus {
    /// `~/.claude/.credentials.json` が存在するか (`/login` 済みの目安)。
    pub credentials_file: bool,
    /// env `CLAUDE_CODE_OAUTH_TOKEN` が非空で設定されているか (`setup-token` 経路)。
    pub oauth_token_env: bool,
}

impl AuthStatus {
    /// 実環境から判定する。
    pub fn probe() -> Self {
        Self::from_parts(
            credentials_path().map(|p| p.is_file()).unwrap_or(false),
            std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN"),
        )
    }

    /// 判定本体 (テスト注入点)。token の値はここで即 bool 化され、保持されない。
    pub fn from_parts(credentials_file: bool, token: Option<OsString>) -> Self {
        Self {
            credentials_file,
            oauth_token_env: token.map(|v| !v.is_empty()).unwrap_or(false),
        }
    }

    /// hello / pong 応答に埋める JSON。boolean のみで構成する (値を含めない)。
    pub fn to_json(self) -> Value {
        json!({
            "credentials_file": self.credentials_file,
            "oauth_token_env": self.oauth_token_env,
            "likely_logged_in": self.credentials_file || self.oauth_token_env,
        })
    }
}

/// credentials ファイルのパス。`%USERPROFILE%\.claude\.credentials.json` (Windows) /
/// `$HOME/.claude/.credentials.json`。
fn credentials_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| credentials_path_under(Path::new(&h)))
}

/// home 配下の credentials パスを組み立てる純関数。
pub fn credentials_path_under(home: &Path) -> PathBuf {
    home.join(".claude").join(".credentials.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_contains_only_booleans() {
        // 値 (token) を渡しても、出力 JSON は boolean 3 キーだけで構成される。
        let secret = OsString::from("sk-ant-oat01-DUMMY-SECRET-VALUE");
        let v = AuthStatus::from_parts(true, Some(secret)).to_json();
        let obj = v.as_object().expect("object になるはず");
        assert_eq!(obj.len(), 3);
        for (k, val) in obj {
            assert!(val.is_boolean(), "{k} が boolean でない: {val}");
        }
        // secret の値が serialize 結果に一切現れない。
        assert!(!v.to_string().contains("DUMMY-SECRET"));
    }

    #[test]
    fn likely_logged_in_is_or_of_sources() {
        let cases = [
            (false, None, false),
            (true, None, true),
            (false, Some(OsString::from("tok")), true),
            (true, Some(OsString::from("tok")), true),
        ];
        for (cred, token, expect) in cases {
            let v = AuthStatus::from_parts(cred, token).to_json();
            assert_eq!(v["likely_logged_in"], expect);
        }
    }

    #[test]
    fn empty_env_token_counts_as_absent() {
        let s = AuthStatus::from_parts(false, Some(OsString::new()));
        assert!(!s.oauth_token_env);
        assert_eq!(s.to_json()["likely_logged_in"], false);
    }

    #[test]
    fn credentials_path_layout() {
        let p = credentials_path_under(Path::new("/home/me"));
        assert!(p.ends_with(".claude/.credentials.json"));
    }

    #[test]
    fn probe_detects_credentials_file() {
        // 実ファイルの有無で from_parts 入力が変わることを、tempdir で固定する
        // (env 依存を避けるため probe そのものではなく path 判定を検証)。
        let dir = std::env::temp_dir().join(format!("ccwr-auth-test-{}", std::process::id()));
        let claude_dir = dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let cred = credentials_path_under(&dir);
        assert!(!cred.is_file());
        std::fs::write(&cred, "{}").unwrap();
        assert!(cred.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

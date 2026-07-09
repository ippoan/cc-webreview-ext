// SOURCE-MIRROR: ippoan/cdp-relay:rust/agent/src/nmhost.rs (install_native_host 部分)
//! native host manifest の生成・HKCU registry 登録・claude.exe パス解決
//! (cc-webreview-ext#2, #3)。
//!
//! - manifest は user-writable な `%LOCALAPPDATA%\cc-webreview\` に置く (Program Files
//!   直下は admin 無しで書けないため)。
//! - claude.exe のパスは **絶対パス限定**。Chrome が起動する native host は環境変数が
//!   最小限で PATH 解決が信用できないため、`--register --claude-path <abs>` で
//!   `HKCU\Software\ippoan\cc-webreview` の `ClaudeExe` 値に保存し、起動時に読む。
//! - manifest 生成・パス検証は OS 非依存の純関数にして CCoW (Linux) で unit test する。

use serde_json::json;
use std::path::{Path, PathBuf};

/// Native Messaging host 名。拡張の `connectNative` の第 1 引数と一致させる。
pub const HOST_NAME: &str = "com.ippoan.cc_webreview";

/// 拡張 ID (extension/manifest.json の `key` から決まる固定 ID)。allowed_origins に埋める。
pub const EXT_ID: &str = "hkinllfgncahghgkimjjcdppgnglijcb";

/// claude パス等を保存する registry key (HKCU 配下)。
#[cfg(windows)]
const CONFIG_KEY: &str = r"Software\ippoan\cc-webreview";

/// native-host manifest の JSON を生成する (OS 非依存・純関数)。`exe_path` は host exe の
/// 絶対パス、`ext_id` は許可する拡張 ID。
pub fn native_host_manifest_json(exe_path: &Path, ext_id: &str) -> String {
    let manifest = json!({
        "name": HOST_NAME,
        "description": "cc-webreview native host (claude launcher)",
        "path": exe_path.to_string_lossy().into_owned(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{ext_id}/")],
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| manifest.to_string())
}

/// `--claude-path` の値を検証する純関数。絶対パスのみ受け付ける (PATH 依存禁止)。
pub fn validate_claude_path(p: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(p);
    if !path.is_absolute() {
        return Err(format!("claude パスは絶対パスで指定してください: {p}"));
    }
    // 実在しないパスは reject する — README の例文プレースホルダ
    // (`C:\path\to\claude.exe`) がそのまま登録され resolve が壊れた実害があるため、
    // 登録時点で loud fail させる。
    if !path.is_file() {
        return Err(format!(
            "claude が {p} に存在しません。`where.exe claude` で実パスを確認してから --register し直してください"
        ));
    }
    Ok(path)
}

/// manifest / 状態ファイルを置く user-writable ディレクトリ。
/// `%LOCALAPPDATA%\cc-webreview`、無ければ (非 Windows 等) `$HOME/.cc-webreview`。
pub fn data_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("cc-webreview"))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cc-webreview")))
        .ok_or_else(|| "LOCALAPPDATA も HOME も不明".to_string())
}

/// claude spawn 用の安定 work dir (`data_dir()/work`)。Chrome から継承する cwd は
/// 起動経路で変わり、claude の trust プロンプトが毎回出る原因になる (#28) —
/// cwd 未指定の spawn はこの dir に固定する。作成失敗時は None (従来どおり cwd 継承)。
pub fn default_work_dir() -> Option<PathBuf> {
    let dir = data_dir().ok()?.join("work");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// manifest を書き、Chrome / Edge の HKCU registry に登録する (Windows)。admin 不要。
/// `claude_path` を渡した場合は `ClaudeExe` 値にも保存する。
#[cfg(windows)]
pub fn install(claude_path: Option<&Path>, ext_id: &str) -> Result<String, String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let manifest_path = dir.join(format!("{HOST_NAME}.json"));
    std::fs::write(&manifest_path, native_host_manifest_json(&exe, ext_id))
        .map_err(|e| e.to_string())?;

    let manifest_str = manifest_path.to_string_lossy().into_owned();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for base in [
        r"Software\Google\Chrome\NativeMessagingHosts",
        r"Software\Microsoft\Edge\NativeMessagingHosts",
    ] {
        let (key, _) = hkcu
            .create_subkey(format!(r"{base}\{HOST_NAME}"))
            .map_err(|e| format!("registry {base}: {e}"))?;
        key.set_value("", &manifest_str)
            .map_err(|e| format!("registry set {base}: {e}"))?;
    }
    if let Some(p) = claude_path {
        let (key, _) = hkcu
            .create_subkey(CONFIG_KEY)
            .map_err(|e| format!("registry {CONFIG_KEY}: {e}"))?;
        key.set_value("ClaudeExe", &p.to_string_lossy().into_owned())
            .map_err(|e| format!("registry set ClaudeExe: {e}"))?;
    }
    Ok(format!("native host 登録: {manifest_str}"))
}

/// 非 Windows fallback。registry 登録は Windows 専用 (manifest 生成のみ可能)。
#[cfg(not(windows))]
pub fn install(_claude_path: Option<&Path>, _ext_id: &str) -> Result<String, String> {
    Err(format!(
        "native-host 登録は Windows のみ対応 (host={HOST_NAME})"
    ))
}

/// claude 実行ファイルの絶対パスを解決する。優先順:
/// 1. env `CC_WEBREVIEW_CLAUDE` (開発 / CCoW テスト用)
/// 2. registry `HKCU\Software\ippoan\cc-webreview` の `ClaudeExe` (Windows)
/// 3. 既知のインストール先候補 (`%USERPROFILE%\.local\bin\claude.exe` 等)
pub fn resolve_claude_path() -> Option<PathBuf> {
    resolve_claude_path_with_source().map(|(p, _)| p)
}

/// `resolve_claude_path` と同じ優先順で、どの経路で解決したかのラベルも返す
/// (#31 診断用。side panel の「診断」表示に使う)。
pub fn resolve_claude_path_with_source() -> Option<(PathBuf, &'static str)> {
    if let Some(p) = std::env::var_os("CC_WEBREVIEW_CLAUDE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some((p, "env CC_WEBREVIEW_CLAUDE"));
        }
    }
    if let Some(p) = claude_path_from_registry() {
        if p.is_file() {
            return Some((p, "registry ClaudeExe"));
        }
    }
    default_claude_candidates()
        .into_iter()
        .find(|cand| cand.is_file())
        .map(|p| (p, "既定のインストール先"))
}

#[cfg(windows)]
fn claude_path_from_registry() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(CONFIG_KEY).ok()?;
    let v: String = key.get_value("ClaudeExe").ok()?;
    Some(PathBuf::from(v))
}

#[cfg(not(windows))]
fn claude_path_from_registry() -> Option<PathBuf> {
    None
}

/// 既知のインストール先候補 (環境変数から絶対パスを組み立てる。PATH は見ない)。
fn default_claude_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        v.push(home.join(".local").join("bin").join("claude.exe"));
        v.push(home.join(".local").join("bin").join("claude"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_json_has_required_fields() {
        let s = native_host_manifest_json(
            &PathBuf::from(r"C:\Users\me\bin\cc-webreview-agent.exe"),
            EXT_ID,
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["name"], HOST_NAME);
        assert_eq!(v["type"], "stdio");
        assert_eq!(
            v["allowed_origins"][0],
            format!("chrome-extension://{EXT_ID}/")
        );
        assert!(v["path"]
            .as_str()
            .unwrap()
            .ends_with("cc-webreview-agent.exe"));
    }

    #[test]
    fn validate_claude_path_requires_absolute_and_existing() {
        // 相対パスは reject。
        assert!(validate_claude_path("claude.exe").is_err());
        assert!(validate_claude_path("bin/claude").is_err());

        // 絶対パスでも実在しなければ reject (README のプレースホルダ登録事故の再発防止)。
        #[cfg(windows)]
        assert!(validate_claude_path(r"C:\path\to\claude.exe").is_err());
        #[cfg(not(windows))]
        assert!(validate_claude_path("/path/to/claude").is_err());

        // 実在する絶対パスは OK (tempfile で固定)。
        let dir = std::env::temp_dir().join(format!("ccwr-reg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("claude.exe");
        std::fs::write(&exe, b"stub").unwrap();
        assert!(validate_claude_path(&exe.to_string_lossy()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

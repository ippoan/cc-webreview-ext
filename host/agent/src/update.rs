// SOURCE-MIRROR: ippoan/cdp-relay:rust/agent/src/update.rs
//! agent 内蔵 self-update + 同梱拡張の更新 (cc-webreview-ext#6)。
//!
//! 起動時に GitHub Releases を見て `agent-dev-N` の最新を解決し、自分より新しければ
//! Windows zip asset を DL → minisign 署名を検証 → 中の `cc-webreview-agent.exe` を
//! 取り出して self_replace で差し替える (実行中プロセスは旧版のまま、次回起動で反映)。
//!
//! version 比較 / asset 選択 / release JSON 解釈 / 署名検証の純ロジックは CCoW で
//! unit test する。実 DL / 置換は Windows 手元でのみ動く (CCoW では走らせない)。
//!
//! REST レート (anonymous 60/hr) は手元 1 人なら起動毎チェックで十分収まる。tag は
//! build.rs が埋め込む CC_WEBREVIEW_RELEASE_TAG。ローカル dev ビルド (tag 無し) は
//! 自動更新しない (= 開発中の自分を上書きしない)。

use serde_json::Value;
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

const RELEASES_API: &str =
    "https://api.github.com/repos/ippoan/cc-webreview-ext/releases?per_page=100";
const TAG_PREFIX: &str = "agent-dev-";
const WIN_ASSET_MARK: &str = "x86_64-pc-windows-msvc";
const USER_AGENT: &str = "cc-webreview-agent-self-update";

/// asset DL を許可する host (GitHub Releases とその CDN のみ)。asset_url は
/// `ippoan/cc-webreview-ext` の releases API (TLS, host 固定) が返すものだが、SSRF /
/// 偽 host への redirect を防ぐ defense-in-depth として host を pin する。
const ASSET_HOST_ALLOWLIST: &[&str] = &["github.com", "objects.githubusercontent.com"];
/// DL / 展開のサイズ上限 (zip bomb / 無制限 DL 対策)。実 release zip より十分大きい。
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
/// detached 署名 (`.minisig`) の DL サイズ上限。minisign の .minisig は数百 byte なので
/// 十分すぎる小さい cap を被せる (誤った巨大 body の掴み込み防止)。
const MAX_SIG_BYTES: u64 = 16 * 1024;

/// self-update asset の検証に使う minisign 公開鍵 (base64 1 行)。
///
/// **cdp-relay と共用の org 鍵** (#6 は当初「新規生成」だったが、secret 数を増やさない
/// user 判断で共用に変更)。対応する秘密鍵は org secret `MINISIGN_SECRET_KEY` にあり、
/// release workflow が各 zip を署名して `<asset>.minisig` を Release に添付する。
/// 公開鍵は秘密ではないのでここに hard-code してよい (= GitHub アカウント /
/// トークン侵害で偽 asset が release に乗っても、この鍵で署名できなければ
/// self_replace に進まない、という supply-chain 防御層)。
const MINISIGN_PUBLIC_KEY: &str = "RWSasFZdc3W2IqbOY7FEsZ7MIhwqiFzs+0vpdtEZ2KqrZOUzl+YOEZ9W";

/// build.rs が埋め込んだ現在のリリース tag。ローカル dev ビルドでは None。
pub fn current_release_tag() -> Option<&'static str> {
    option_env!("CC_WEBREVIEW_RELEASE_TAG")
}

/// `agent-dev-7` → 7。prefix 不一致や非数値は None。
fn dev_counter(tag: &str) -> Option<u64> {
    tag.strip_prefix(TAG_PREFIX)?.parse().ok()
}

/// 更新候補。
#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub tag: String,
    pub asset_url: String,
}

/// releases JSON (api の配列) から「current より新しい最大 dev-N + その Windows zip asset」を選ぶ。
/// current が None (dev ビルド) なら更新しない。Windows asset が無い release は飛ばす。
pub fn pick_newer(current: Option<&str>, releases: &Value) -> Option<Candidate> {
    let current_n = dev_counter(current?)?; // dev ビルドや解釈不能は更新しない
    let arr = releases.as_array()?;

    let mut best: Option<(u64, Candidate)> = None;
    for rel in arr {
        let tag = rel.get("tag_name").and_then(Value::as_str).unwrap_or("");
        let Some(n) = dev_counter(tag) else { continue };
        if n <= current_n {
            continue;
        }
        let Some(asset_url) = pick_windows_asset(rel) else {
            continue;
        };
        let better = match &best {
            Some((bn, _)) => n > *bn,
            None => true,
        };
        if better {
            best = Some((
                n,
                Candidate {
                    tag: tag.to_string(),
                    asset_url,
                },
            ));
        }
    }
    best.map(|(_, c)| c)
}

/// release の assets から条件 `pick` に合う zip を探し、**同じ release に detached
/// 署名 `<name>.minisig` が添付されている場合のみ** browser_download_url を返す。
/// 署名導入前の古い release は選択段階で飛ばす — DL してから .minisig 404 で落とす
/// より診断が明確で、余計な DL も発生しない (実害: agent-dev-8 掴みで 404)。
fn signed_asset_url(release: &Value, pick: impl Fn(&str) -> bool) -> Option<String> {
    let assets = release.get("assets")?.as_array()?;
    let (name, url) = assets.iter().find_map(|a| {
        let name = a.get("name").and_then(Value::as_str).unwrap_or("");
        if !pick(name) {
            return None;
        }
        a.get("browser_download_url")
            .and_then(Value::as_str)
            .map(|u| (name.to_string(), u.to_string()))
    })?;
    let sig_name = format!("{name}.minisig");
    let has_sig = assets
        .iter()
        .any(|a| a.get("name").and_then(Value::as_str) == Some(sig_name.as_str()));
    has_sig.then_some(url)
}

/// release の assets から Windows msvc zip (署名付き) の browser_download_url を拾う。
fn pick_windows_asset(release: &Value) -> Option<String> {
    signed_asset_url(release, |name| {
        name.contains(WIN_ASSET_MARK) && name.ends_with(".zip")
    })
}

/// asset URL を https + host allowlist で検証する (SSRF / 偽 host への置換を防ぐ)。
fn validate_asset_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("asset url parse: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("asset url must be https".into());
    }
    match parsed.host_str() {
        Some(h) if ASSET_HOST_ALLOWLIST.contains(&h) => Ok(()),
        Some(h) => Err(format!("asset host not allowed: {h}")),
        None => Err("asset url has no host".into()),
    }
}

/// detached minisign 署名 (`.minisig` の中身) で `data` を検証する純ロジック。
/// 公開鍵は `MINISIGN_PUBLIC_KEY` 固定。検証失敗 (署名不一致 / 別鍵 / 壊れた署名) は Err。
///
/// minisign 0.11+ の prehashed 署名を想定するので legacy (`allow_legacy=false`)。
fn verify_minisign(data: &[u8], minisig: &str) -> Result<(), String> {
    let pk = minisign_verify::PublicKey::from_base64(MINISIGN_PUBLIC_KEY)
        .map_err(|e| format!("公開鍵 parse 失敗: {e}"))?;
    let sig = minisign_verify::Signature::decode(minisig)
        .map_err(|e| format!("署名 decode 失敗: {e}"))?;
    pk.verify(data, &sig, false)
        .map_err(|e| format!("署名検証失敗: {e}"))
}

/// asset (`url`) に対応する `{url}.minisig` を DL して `data` を検証する。
/// host allowlist / https / サイズ cap は asset 本体と同じガードを通す。
fn verify_asset_signature(agent: &ureq::Agent, asset_url: &str, data: &[u8]) -> Result<(), String> {
    let sig_url = format!("{asset_url}.minisig");
    validate_asset_url(&sig_url)?;
    let resp = agent
        .get(&sig_url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!(".minisig DL 失敗 (署名未添付の release か): {e}"))?;
    let mut sig_bytes: Vec<u8> = Vec::new();
    resp.into_reader()
        .take(MAX_SIG_BYTES + 1)
        .read_to_end(&mut sig_bytes)
        .map_err(|e| e.to_string())?;
    if sig_bytes.len() as u64 > MAX_SIG_BYTES {
        return Err(".minisig が上限サイズを超過".into());
    }
    let sig_text = String::from_utf8(sig_bytes).map_err(|e| format!(".minisig が非 UTF-8: {e}"))?;
    verify_minisign(data, &sig_text)
}

fn build_agent() -> ureq::Agent {
    ureq::builder()
        .tls_connector(Arc::new(
            native_tls::TlsConnector::new().expect("native-tls init"),
        ))
        .timeout(Duration::from_secs(30))
        .build()
}

/// releases API を叩いて JSON 配列を返す (agent 本体 / 拡張更新で共用)。
fn fetch_releases(agent: &ureq::Agent) -> Result<Value, String> {
    let body = agent
        .get(RELEASES_API)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("releases 取得失敗: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// 起動時チェック本体。新版があれば DL + 差し替えし、適用した tag を返す。
/// 更新不要 / dev ビルド / 取得失敗は Ok(None) or Err (呼び出し側で log するだけ)。
pub fn check_and_self_update() -> Result<Option<String>, String> {
    let Some(current) = current_release_tag() else {
        return Ok(None); // dev ビルドは自動更新しない
    };
    let agent = build_agent();
    let releases = fetch_releases(&agent)?;

    let Some(cand) = pick_newer(Some(current), &releases) else {
        return Ok(None); // 最新
    };
    download_and_replace(&agent, &cand.asset_url)?;
    Ok(Some(cand.tag))
}

/// zip asset を DL → 署名検証 → 中の cc-webreview-agent.exe を temp に取り出して
/// self_replace で現 exe を差し替える。
fn download_and_replace(agent: &ureq::Agent, url: &str) -> Result<(), String> {
    let bytes = download_asset(agent, url)?;

    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("zip 展開失敗: {e}"))?;

    // zip 内の cc-webreview-agent.exe を探す。
    let mut exe_index = None;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name();
        if name.ends_with("cc-webreview-agent.exe") || name.ends_with("cc-webreview-agent") {
            exe_index = Some(i);
            break;
        }
    }
    let idx = exe_index.ok_or_else(|| "zip 内に cc-webreview-agent.exe が無い".to_string())?;

    let tmp = std::env::temp_dir().join("cc-webreview-agent-update.tmp");
    {
        let entry = zip.by_index(idx).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        // 展開も上限で cap (zip bomb 対策)。
        let written = std::io::copy(&mut entry.take(MAX_ASSET_BYTES + 1), &mut out)
            .map_err(|e| e.to_string())?;
        if written > MAX_ASSET_BYTES {
            let _ = std::fs::remove_file(&tmp);
            return Err("展開後サイズが上限超過".into());
        }
    }
    self_replace::self_replace(&tmp).map_err(|e| format!("self_replace 失敗: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// asset を DL してサイズ cap を検査し、**中身を触る前に minisign 署名を検証**して返す。
/// 検証に通らない asset は zip を開かず破棄 (= GitHub アカウント / トークン侵害で
/// 偽 asset が乗っても展開・置換に進まない)。agent 本体 / 拡張 zip 共通のガード。
fn download_asset(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    validate_asset_url(url)?;
    let resp = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("asset DL 失敗: {e}"))?;
    // zip bomb / 無制限 DL 対策で上限を被せる。上限到達は truncate せず reject。
    let mut bytes: Vec<u8> = Vec::new();
    resp.into_reader()
        .take(MAX_ASSET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return Err("asset が上限サイズを超過".into());
    }
    verify_asset_signature(agent, url, &bytes)?;
    Ok(bytes)
}

// ─── 拡張 (unpacked) の自動更新 ─────────────────────────────────────────────
//
// unpacked 拡張は Chrome が自動更新しない。そこで agent が GitHub の最新拡張 zip を
// install dir の extension\ に上書きし、Chrome 起動/再起動時に新版を読ませる
// (= 実質自動更新)。side panel には {type:"update"} で通知し、リロード導線を出す。

/// 拡張 zip asset の名前 prefix (release.yml が `cc-webreview-extension-<tag>.zip` で出す)。
const EXT_ASSET_PREFIX: &str = "cc-webreview-extension-";

/// releases から最新 (dev-N 最大) の署名付き拡張 zip asset を選ぶ。
///
/// **API の配列順に依存しない。** GitHub の /releases はタグ名の逆辞書順で返ることが
/// あり (実測: agent-dev-8 が agent-dev-19 より先頭)、「先頭 = 最新」の仮定は壊れる。
/// agent 本体の pick_newer と同じく dev counter の最大値で選ぶ (= dev-N tag のみ対象)。
pub fn pick_latest_extension(releases: &Value) -> Option<(String, String)> {
    let arr = releases.as_array()?;
    let mut best: Option<(u64, String, String)> = None;
    for rel in arr {
        let tag = rel.get("tag_name").and_then(Value::as_str).unwrap_or("");
        let Some(n) = dev_counter(tag) else { continue };
        let Some(url) = signed_asset_url(rel, |name| {
            name.starts_with(EXT_ASSET_PREFIX) && name.ends_with(".zip")
        }) else {
            continue;
        };
        if best.as_ref().map(|(bn, _, _)| n > *bn).unwrap_or(true) {
            best = Some((n, tag.to_string(), url));
        }
    }
    best.map(|(_, tag, url)| (tag, url))
}

/// ディスクに適用済みの拡張 tag (.ext-version) を読む — **GitHub API は使わない**。
/// hello / pong に載せ、panel が「動作中の拡張が古い = リロード待ち」を
/// ローカル比較 (動作中 manifest version との突合) だけで判定できるようにする。
/// 新リリースの発見は従来どおり host 起動時のバックグラウンドチェックが担う。
pub fn applied_extension_tag() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let marker = exe.parent()?.join("extension").join(".ext-version");
    let s = std::fs::read_to_string(marker).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// install dir の extension\ を最新拡張 zip で更新する。前回 tag は .ext-version に記録。
/// extension\ が無い (dev / 手動 exe) なら何もしない。
pub fn update_extension() -> Result<Option<String>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ext_dir = exe
        .parent()
        .ok_or_else(|| "exe parent 不明".to_string())?
        .join("extension");
    if !ext_dir.is_dir() {
        return Ok(None); // MSI 同梱拡張が無い
    }
    let marker = ext_dir.join(".ext-version");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    let current = current.trim();

    let agent = build_agent();
    let releases = fetch_releases(&agent)?;

    let Some((tag, url)) = pick_latest_extension(&releases) else {
        return Ok(None);
    };
    if !current.is_empty() && tag == current {
        return Ok(None); // 最新
    }
    download_extension(&agent, &url, &ext_dir)?;
    let _ = std::fs::write(&marker, &tag);
    Ok(Some(tag))
}

/// 拡張 zip を DL → 署名検証 → ext_dir に展開する (flat 構成のみ、path traversal は弾く)。
/// cdp-relay 版と違い拡張 zip も minisign 検証を必須にする (install dir に書き込む
/// 内容は全て署名済みに揃える)。
fn download_extension(
    agent: &ureq::Agent,
    url: &str,
    ext_dir: &std::path::Path,
) -> Result<(), String> {
    let bytes = download_asset(agent, url)?;
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("ext zip: {e}"))?;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        // 拡張は flat (manifest.json 等)。サブディレクトリ / traversal は弾く。
        if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
            continue;
        }
        let out_path = ext_dir.join(&name);
        let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry.take(MAX_ASSET_BYTES + 1), &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dev_counter_parses_and_rejects() {
        assert_eq!(dev_counter("agent-dev-7"), Some(7));
        assert_eq!(dev_counter("agent-dev-0"), Some(0));
        assert_eq!(dev_counter("agent-v1.2.3"), None);
        assert_eq!(dev_counter("dev"), None);
        assert_eq!(dev_counter("agent-dev-x"), None);
    }

    /// asset + その `.minisig` を添付した release fixture (署名付きが正常形)。
    fn rel(tag: &str, asset: Option<&str>) -> Value {
        let assets = match asset {
            Some(name) => json!([
                {
                    "name": name,
                    "browser_download_url": format!("https://example.com/{name}")
                },
                {
                    "name": format!("{name}.minisig"),
                    "browser_download_url": format!("https://example.com/{name}.minisig")
                }
            ]),
            None => json!([]),
        };
        json!({ "tag_name": tag, "assets": assets })
    }

    /// `.minisig` の無い (署名導入前の) release fixture。
    fn rel_unsigned(tag: &str, asset: &str) -> Value {
        json!({ "tag_name": tag, "assets": [{
            "name": asset,
            "browser_download_url": format!("https://example.com/{asset}")
        }]})
    }

    #[test]
    fn pick_newer_returns_highest_with_windows_asset() {
        let releases = json!([
            rel(
                "agent-dev-5",
                Some("cc-webreview-agent-agent-dev-5-x86_64-pc-windows-msvc.zip")
            ),
            rel(
                "agent-dev-7",
                Some("cc-webreview-agent-agent-dev-7-x86_64-pc-windows-msvc.zip")
            ),
            rel(
                "agent-dev-6",
                Some("cc-webreview-agent-agent-dev-6-x86_64-pc-windows-msvc.zip")
            ),
        ]);
        let c = pick_newer(Some("agent-dev-5"), &releases).unwrap();
        assert_eq!(c.tag, "agent-dev-7");
        assert!(c.asset_url.ends_with("dev-7-x86_64-pc-windows-msvc.zip"));
    }

    #[test]
    fn pick_newer_none_when_up_to_date() {
        let releases = json!([rel(
            "agent-dev-7",
            Some("cc-webreview-agent-agent-dev-7-x86_64-pc-windows-msvc.zip")
        )]);
        assert!(pick_newer(Some("agent-dev-7"), &releases).is_none());
        // 自分より古いだけ
        assert!(pick_newer(Some("agent-dev-9"), &releases).is_none());
    }

    #[test]
    fn pick_newer_skips_release_without_windows_asset() {
        let releases = json!([
            rel(
                "agent-dev-8",
                Some("cc-webreview-agent-agent-dev-8-x86_64-unknown-linux-gnu.tar.gz")
            ),
            rel(
                "agent-dev-7",
                Some("cc-webreview-agent-agent-dev-7-x86_64-pc-windows-msvc.zip")
            ),
        ]);
        // dev-8 は linux only なので飛ばし、dev-7 を選ぶ。
        let c = pick_newer(Some("agent-dev-6"), &releases).unwrap();
        assert_eq!(c.tag, "agent-dev-7");
    }

    #[test]
    fn pick_newer_none_for_dev_build() {
        let releases = json!([rel(
            "agent-dev-7",
            Some("cc-webreview-agent-agent-dev-7-x86_64-pc-windows-msvc.zip")
        )]);
        // current None (ローカル dev) は自動更新しない。
        assert!(pick_newer(None, &releases).is_none());
    }

    #[test]
    fn pick_newer_ignores_extension_zip() {
        // 拡張 zip は msvc mark を含まないので agent asset として選ばれない。
        let releases = json!([rel(
            "agent-dev-8",
            Some("cc-webreview-extension-agent-dev-8.zip")
        )]);
        assert!(pick_newer(Some("agent-dev-7"), &releases).is_none());
    }

    #[test]
    fn validate_asset_url_allows_github_https() {
        assert!(validate_asset_url(
            "https://github.com/ippoan/cc-webreview-ext/releases/download/agent-dev-7/x.zip"
        )
        .is_ok());
        assert!(validate_asset_url("https://objects.githubusercontent.com/abc/def").is_ok());
    }

    #[test]
    fn validate_asset_url_rejects_bad_host_and_scheme() {
        assert!(validate_asset_url("https://evil.example.com/x.zip").is_err());
        assert!(validate_asset_url("http://github.com/x.zip").is_err());
        assert!(validate_asset_url("not-a-url").is_err());
    }

    #[test]
    fn pick_latest_extension_ignores_api_order_and_unsigned() {
        // GitHub /releases はタグ名の逆辞書順で返ることがある (実測: dev-8 が dev-19
        // より先頭)。配列順に依存せず dev-N 最大の**署名付き** release を選ぶこと。
        let releases = json!([
            // 先頭 = 署名なしの古い dev-8 (実害を再現) → 選ばない
            rel_unsigned("agent-dev-8", "cc-webreview-extension-agent-dev-8.zip"),
            rel(
                "agent-dev-17",
                Some("cc-webreview-extension-agent-dev-17.zip")
            ),
            rel(
                "agent-dev-19",
                Some("cc-webreview-extension-agent-dev-19.zip")
            ),
        ]);
        let (tag, url) = pick_latest_extension(&releases).unwrap();
        assert_eq!(tag, "agent-dev-19");
        assert!(url.ends_with("cc-webreview-extension-agent-dev-19.zip"));
    }

    #[test]
    fn pick_latest_extension_none_when_no_signed_extension_asset() {
        let releases = json!([
            { "tag_name": "agent-dev-9", "assets": [
                { "name": "cc-webreview-agent-0.0.9-x86_64.msi", "browser_download_url": "u" }
            ]},
            // 拡張 zip はあるが署名なし → 対象外
            rel_unsigned("agent-dev-8", "cc-webreview-extension-agent-dev-8.zip"),
        ]);
        assert!(pick_latest_extension(&releases).is_none());
    }

    // minisign 署名検証のテストベクタ。`MINISIGN_PUBLIC_KEY` (本番鍵) とは別の使い捨て
    // テスト鍵で `TEST_PAYLOAD` を署名したもの (テスト秘密鍵は生成後に破棄済み)。
    // 検証ロジックの形式互換 (prehashed, legacy=false) を pin する。本番鍵の秘密鍵は
    // リポジトリに無いので、テストでは verify_minisign と同じ呼び出し形の薄いヘルパ
    // verify_with をテスト公開鍵で呼ぶ。
    const TEST_PUBKEY: &str = "RWRwoULYKfksqozNYIHTYDcHGeB6vXYzQBeazLDyMtpTrf+NCUdaGOL9";
    const TEST_PAYLOAD: &[u8] = b"hello cc-webreview update";
    const TEST_MINISIG: &str = "untrusted comment: signature from minisign secret key\n\
RURwoULYKfksqtmWYiFaPDRHr/9uKoZ+gd1ozqfVeIhKc2kUqA1Pk3dNyR7Yh3IvPxdNr1KpdB69ffoRvS3P7rtpglPFbwcyYAw=\n\
trusted comment: timestamp:1783530777\tfile:payload.bin\thashed\n\
R8AVSQq5/+oA2bMF8/pxJPlmZEknex10VYCLhuG28QGS6njYk7Swv3CP6IetVdDHPwcRhRDKgFV+GsIuQXsbCw==\n";

    /// テスト用: 任意の公開鍵で検証する (verify_minisign は本番鍵固定なので、テスト鍵を
    /// 使うためのヘルパ。本番ロジックと同じ呼び出し形 (prehashed, legacy=false) を保つ)。
    fn verify_with(pubkey: &str, data: &[u8], minisig: &str) -> Result<(), String> {
        let pk = minisign_verify::PublicKey::from_base64(pubkey)
            .map_err(|e| format!("公開鍵 parse 失敗: {e}"))?;
        let sig = minisign_verify::Signature::decode(minisig)
            .map_err(|e| format!("署名 decode 失敗: {e}"))?;
        pk.verify(data, &sig, false)
            .map_err(|e| format!("署名検証失敗: {e}"))
    }

    #[test]
    fn verify_minisign_accepts_valid_signature() {
        assert!(verify_with(TEST_PUBKEY, TEST_PAYLOAD, TEST_MINISIG).is_ok());
    }

    #[test]
    fn verify_minisign_rejects_tampered_data() {
        // 1 byte でも変われば検証は失敗する。
        let mut tampered = TEST_PAYLOAD.to_vec();
        tampered[0] ^= 0x01;
        assert!(verify_with(TEST_PUBKEY, &tampered, TEST_MINISIG).is_err());
    }

    #[test]
    fn verify_minisign_rejects_wrong_public_key() {
        // 本番鍵 (別鍵) では テスト署名は通らない = 偽 asset を弾けることの証明。
        assert!(verify_with(MINISIGN_PUBLIC_KEY, TEST_PAYLOAD, TEST_MINISIG).is_err());
    }

    #[test]
    fn verify_minisign_rejects_malformed_signature() {
        assert!(verify_with(TEST_PUBKEY, TEST_PAYLOAD, "not a minisig").is_err());
    }

    #[test]
    fn production_public_key_is_valid() {
        // hard-code した本番公開鍵が parse 可能であること (typo 検出)。
        assert!(minisign_verify::PublicKey::from_base64(MINISIGN_PUBLIC_KEY).is_ok());
    }

    #[test]
    fn pick_windows_asset_prefers_zip_msvc_and_requires_minisig() {
        let r = json!({
            "assets": [
                { "name": "cc-webreview-agent-dev-7-x86_64-unknown-linux-gnu.tar.gz", "browser_download_url": "u1" },
                { "name": "cc-webreview-agent-dev-7-x86_64-pc-windows-msvc.zip", "browser_download_url": "u2" },
                { "name": "cc-webreview-agent-dev-7-x86_64-pc-windows-msvc.zip.minisig", "browser_download_url": "u2-sig" }
            ]
        });
        assert_eq!(pick_windows_asset(&r).as_deref(), Some("u2"));

        // .minisig が無い release は掴まない (署名導入前の古い release を除外)。
        let unsigned = json!({
            "assets": [
                { "name": "cc-webreview-agent-dev-7-x86_64-pc-windows-msvc.zip", "browser_download_url": "u2" }
            ]
        });
        assert!(pick_windows_asset(&unsigned).is_none());
    }

    #[test]
    fn pick_newer_skips_unsigned_release() {
        // 新しい dev-9 が署名なしなら、署名付きの dev-8 に落とす (掴んで 404 しない)。
        let releases = json!([
            rel_unsigned(
                "agent-dev-9",
                "cc-webreview-agent-agent-dev-9-x86_64-pc-windows-msvc.zip"
            ),
            rel(
                "agent-dev-8",
                Some("cc-webreview-agent-agent-dev-8-x86_64-pc-windows-msvc.zip")
            ),
        ]);
        let c = pick_newer(Some("agent-dev-7"), &releases).unwrap();
        assert_eq!(c.tag, "agent-dev-8");
    }
}

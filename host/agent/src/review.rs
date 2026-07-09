//! レビュープロンプトテンプレートの差し込み (cc-webreview-ext#5)。
//!
//! テンプレは repo 管理の `host/prompts/review.md` を include_str! でバイナリに同梱する
//! (拡張 zip / MSI のパッケージング変更が不要で、テンプレ更新は agent self-update に
//! 相乗りする)。拡張の `{cmd:"review_prompt", pr:{...}}` に対し、差し込み済みプロンプトと
//! `-p` 用の最小 allowlist を `{type:"review_prompt"}` で返す。

use serde_json::Value;

/// レビュープロンプトのテンプレート (repo 管理: `host/prompts/review.md`)。
pub const REVIEW_TEMPLATE: &str = include_str!("../../prompts/review.md");

/// `{cmd:"review_prompt"}` の `pr` フィールド。
#[derive(Debug, PartialEq, Default)]
pub struct PrInfo {
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub title: String,
    pub author: String,
}

/// `pr` オブジェクトを parse する (純関数)。number は数値/文字列どちらも受ける
/// (ci-dashboard の draft-prs API は数値だが、手書き入力にも耐える)。
pub fn parse_pr_info(v: &Value) -> PrInfo {
    let pr = v.get("pr").cloned().unwrap_or(Value::Null);
    let s = |k: &str| pr.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let number = pr
        .get("number")
        .and_then(|n| {
            n.as_u64()
                .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);
    PrInfo {
        repo: s("repo"),
        number,
        url: s("url"),
        title: s("title"),
        author: s("author"),
    }
}

/// `-p` (headless) レビューの最小 allowlist。read-only + PR コメント投稿のみ。
///
/// `Bash(gh api *)` / `Bash(gh pr *)` の丸ごと許可は**禁止** — `gh api` は任意の
/// write API (PUT/POST/DELETE)、`gh pr` は `close` / `merge` を通してしまい、自己参照
/// レビューで対象 PR を merge/close しうる (docs/plan-review-flow.md 指摘1)。
/// Edit / Write は付与しない (レビュー専用)。ブラウザ系ツールはこの基本 allowlist には
/// 含めず、`--chrome` opt-in 時に `allowlist_with_browser` が `BROWSER_TOOL_RULES` を
/// 追加する (#31)。
///
/// 既知の割り切り (#27 Web Review 5 周目指摘3): コメント投稿の許可は tool 名単位で、
/// **投稿先の PR/issue 番号までは絞れない** — 未信頼 PR タイトル経由の指示注入と
/// 組み合わせると対象外 PR へのスパム投稿は理論上可能 (merge/close は不可能)。
/// 引数レベルの制約は #1 のブラウザ系 allowlist 検討と合わせて再検討する。
pub fn review_allowed_tools() -> Vec<String> {
    [
        "Bash(gh pr view:*)",
        "Bash(gh pr diff:*)",
        "Bash(gh pr checks:*)",
        "Bash(gh pr comment:*)",
        // コメント投稿の優先経路 (ローカル claude に該当 MCP server が構成されて
        // いる場合のみ実在)。MCP は GitHub App の installation token を使うため
        // gh CLI (user PAT) の GraphQL rate limit と別枠 — PAT 枯渇時の詰まりを
        // 回避する (#27 3 周目の実害)。無い環境では単に使われないだけで無害。
        "mcp__githubmcp__add_issue_comment",
        "mcp__github__add_issue_comment",
        "Read",
    ]
    .map(String::from)
    .to_vec()
}

/// `--chrome` 時にレビュー allowlist へ足す browser ツール rule (#31)。
///
/// Claude in Chrome のツールは MCP server **`claude-in-chrome`** として生える
/// (公式 docs: 「Run `/mcp` and select `claude-in-chrome` to see the full list」、
/// https://code.claude.com/docs/en/chrome)。個別ツール名は版で増減し公式一覧も
/// 無いため、server 単位 rule で全ツールを許可する。クリック・入力・遷移を含むが、
/// user が side panel の `--chrome` チェックで明示 opt-in した時だけ効く。
/// headless (`-p`) で実際に provision されるかは #1 spike の Windows 実機検証
/// 項目 — provision されなければ rule は単に使われないだけで無害 (fail-safe)。
pub const BROWSER_TOOL_RULES: &[&str] = &["mcp__claude-in-chrome"];

/// spawn 直前の最終 allowlist を組む (#31)。
///
/// - **空 (非レビューの手動 prompt) は空のまま返す** — `build_claude_args` は空なら
///   `--allowedTools` 自体を付けず無制限のままにする。ここで browser rule を足すと
///   逆に「browser のみ許可」へ restrict してしまい手動利用を壊す。
/// - 非空 (レビュー実行) で `chrome=true` なら browser rule を追加する (重複なし)。
pub fn allowlist_with_browser(tools: Vec<String>, chrome: bool) -> Vec<String> {
    if tools.is_empty() || !chrome {
        return tools;
    }
    let mut out = tools;
    for r in BROWSER_TOOL_RULES {
        if !out.iter().any(|t| t == r) {
            out.push((*r).to_string());
        }
    }
    out
}

/// resume 時に allowlist が空ならレビュー既定を適用する (#27 Web Review 指摘)。
///
/// 拡張の `reviewAllowedTools` はパネル再読み込みで消えるため、「レビュー失敗 →
/// パネルを閉じた → 開き直して 続きから」の典型経路で allowlist が空で届く。
/// 空のまま `-p` を resume すると未許可ツールが全部拒否され gh を一切叩けず
/// 空振りになる (fail-safe 側だが resume が機能しない)。resume は現状レビュー
/// 専用導線なので、空なら read-only 既定を host 側で補う。
pub fn allowlist_or_review_default(tools: Vec<String>) -> Vec<String> {
    if tools.is_empty() {
        review_allowed_tools()
    } else {
        tools
    }
}

/// テンプレに PR 情報を差し込む。単一パス置換 — 差し込んだ値の中にプレースホルダ
/// 文字列が含まれていても再置換しない (PR タイトル経由のテンプレ注入防止)。
/// テンプレ先頭の HTML コメント (repo 読者向けメタ) は取り除く。
pub fn render_review_prompt(pr: &PrInfo) -> String {
    let number = pr.number.to_string();
    let vars: [(&str, &str); 5] = [
        ("{{PR_URL}}", &pr.url),
        ("{{PR_REPO}}", &pr.repo),
        ("{{PR_NUMBER}}", &number),
        ("{{PR_TITLE}}", &pr.title),
        ("{{PR_AUTHOR}}", &pr.author),
    ];
    let template = strip_template_header(REVIEW_TEMPLATE);
    let mut out = String::with_capacity(template.len() + 128);
    let mut rest = template;
    'outer: while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        for (k, v) in &vars {
            if let Some(stripped) = tail.strip_prefix(k) {
                out.push_str(v);
                rest = stripped;
                continue 'outer;
            }
        }
        out.push_str("{{");
        rest = &tail[2..];
    }
    out.push_str(rest);
    out
}

/// テンプレ先頭の `<!-- … -->` コメント 1 個を取り除く (無ければそのまま)。
fn strip_template_header(s: &str) -> &str {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("<!--") {
        if let Some(end) = rest.find("-->") {
            return rest[end + 3..].trim_start();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr() -> PrInfo {
        PrInfo {
            repo: "ippoan/cc-webreview-ext".to_string(),
            number: 26,
            url: "https://github.com/ippoan/cc-webreview-ext/pull/26".to_string(),
            title: "feat: Alt+C ショートカット".to_string(),
            author: "yhonda-ohishi".to_string(),
        }
    }

    #[test]
    fn parses_pr_info() {
        let v = json!({
            "cmd": "review_prompt",
            "pr": {
                "repo": "ippoan/cc-webreview-ext",
                "number": 26,
                "url": "https://github.com/ippoan/cc-webreview-ext/pull/26",
                "title": "feat: Alt+C ショートカット",
                "author": "yhonda-ohishi",
            }
        });
        assert_eq!(parse_pr_info(&v), pr());
        // number は文字列でも受ける
        let v = json!({ "pr": { "number": "26" } });
        assert_eq!(parse_pr_info(&v).number, 26);
        // pr 欠落は default (空) に落ちる
        assert_eq!(parse_pr_info(&json!({})), PrInfo::default());
    }

    #[test]
    fn renders_all_placeholders() {
        let rendered = render_review_prompt(&pr());
        assert!(
            !rendered.contains("{{PR_"),
            "未置換の placeholder が残っている"
        );
        assert!(rendered.contains("ippoan/cc-webreview-ext#26"));
        assert!(rendered.contains("https://github.com/ippoan/cc-webreview-ext/pull/26"));
        assert!(rendered.contains("gh pr view 26 --repo ippoan/cc-webreview-ext"));
        assert!(rendered.contains("yhonda-ohishi"));
        // 冪等マーカーと URL 単独行の出力規約がテンプレに含まれること (指摘2, 5)
        assert!(rendered.contains("<!-- web-review -->"));
        // ブラウザ検証依頼の処理手順 (#31): request 検出 → result 投稿の両 marker と
        // データ変更禁止の制約がテンプレに含まれること
        assert!(rendered.contains("<!-- pr-chat-bridge:request -->"));
        assert!(rendered.contains("<!-- pr-chat-bridge:result -->"));
        assert!(rendered.contains("データ変更を伴う操作"));
        assert!(rendered.contains("URL 単独の行"));
        // Refs 規約 (auto-close 禁止)
        assert!(rendered.contains("Refs #N"));
        // 先頭のテンプレ用メタコメントは取り除かれる
        assert!(!rendered.starts_with("<!--"));
        assert!(!rendered.contains("include_str!"));
    }

    #[test]
    fn injected_values_are_not_resubstituted() {
        // タイトルに placeholder 文字列が入っていても再置換しない (単一パス)
        let mut p = pr();
        p.title = "{{PR_URL}} を試す".to_string();
        let rendered = render_review_prompt(&p);
        assert!(rendered.contains("- タイトル: {{PR_URL}} を試す"));
    }

    #[test]
    fn allowlist_is_minimal_and_readonly() {
        let tools = review_allowed_tools();
        // gh pr サブコマンド限定 + コメント投稿 MCP + Read のみ。
        // 丸ごと許可 / write 系は入れない (指摘1, 6)
        for t in &tools {
            assert!(
                t == "Read" || t.starts_with("Bash(gh pr ") || t.ends_with("__add_issue_comment"),
                "想定外の allowlist entry: {t}"
            );
            assert_ne!(t, "Bash(gh pr:*)", "gh pr 丸ごと許可は禁止");
        }
        assert!(!tools.iter().any(|t| t.contains("gh api")));
        assert!(!tools.iter().any(|t| t == "Edit" || t == "Write"));
        // MCP はコメント投稿 tool のみ (merge / close / issue_write 系は入れない)
        assert!(!tools
            .iter()
            .any(|t| t.contains("merge") || t.contains("issue_write") || t.contains("update")));
        // --allowedTools は comma join で 1 引数に組むため、rule に comma を含めない
        assert!(!tools.iter().any(|t| t.contains(',')));
    }

    #[test]
    fn browser_rules_appended_only_with_chrome() {
        // chrome=true のレビュー allowlist には browser rule が足される (#31)
        let with = allowlist_with_browser(review_allowed_tools(), true);
        for r in BROWSER_TOOL_RULES {
            assert!(with.iter().any(|t| t == r), "{r} が入っていない");
        }
        // 既存の read-only entry はそのまま残る
        for t in review_allowed_tools() {
            assert!(with.contains(&t), "{t} が消えた");
        }
        // chrome=false なら変化なし
        assert_eq!(
            allowlist_with_browser(review_allowed_tools(), false),
            review_allowed_tools()
        );
        // 二重適用しても重複しない (resume 経路で再適用され得る)
        let twice = allowlist_with_browser(with.clone(), true);
        assert_eq!(twice, with);
        // --allowedTools は comma join のため rule に comma を含めない
        assert!(!BROWSER_TOOL_RULES.iter().any(|r| r.contains(',')));
    }

    #[test]
    fn empty_allowlist_stays_unrestricted_with_chrome() {
        // 非レビューの手動 prompt (allowlist 空 = --allowedTools 無し) に browser rule を
        // 足すと「browser のみ許可」へ restrict してしまうため、空は空のまま (#31)
        assert!(allowlist_with_browser(vec![], true).is_empty());
        assert!(allowlist_with_browser(vec![], false).is_empty());
    }

    #[test]
    fn resume_allowlist_defaults_when_empty() {
        // パネル再読み込みで allowed_tools が空で届く resume にはレビュー既定を補う
        assert_eq!(allowlist_or_review_default(vec![]), review_allowed_tools());
        // 明示指定があればそのまま (上書きしない)
        assert_eq!(
            allowlist_or_review_default(vec!["Read".to_string()]),
            vec!["Read".to_string()]
        );
    }
}

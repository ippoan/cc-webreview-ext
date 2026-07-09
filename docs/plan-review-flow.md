# Plan: レビューフロー統合 (issue #5)

Refs #5 / Part of #7 — 実装済み (2026-07-09)。要検討だった項目の決定は各チェック項目に
注記。ブラウザ系 allowlist (#1 spike 依存) のみ未了。

2026-07-09: Web Review (PR #24 コメント) の指摘 6 件を反映。

## ゴール

side panel の draft PR 一覧 (#4、実装済み) から 1 クリックで「ブラウザ込みレビュー →
PR コメント投稿 → CCoW 引き継ぎ」まで無人で走る。

## チェックリスト

### プロンプトテンプレート

- [x] `host/prompts/review.md` としてテンプレートを repo 管理する (sidepanel.js の
      `reviewPrompt()` ハードコードから移す) — **決定: host が `include_str!` で同梱し
      `{cmd:"review_prompt"}` で差し込んで返す** (拡張同梱案は zip / wix / update の
      3 箇所に波及するため不採用。テンプレ更新は agent self-update に相乗り)
- [x] 差し込み変数: PR URL / repo / number / title / author (`{{PR_*}}`、
      `review.rs` の単一パス置換 — 差し込み値経由の再置換なし)
- [x] レビュー観点: diff、CI 状態、必要ならブラウザで PR ページ・プレビュー URL を目視
- [x] 出力の必須タスク化: **PR コメント投稿まで**を完了条件として明記する
- [x] コメント書式: `## Web Review 結果` + 指摘リスト + `## CCoW への引き継ぎ`
      (対応タスクのチェックリスト)。`Refs #N` 規約 (auto-close 禁止) も明記
- [x] **投稿 URL の出力規約 (指摘2)**: テンプレで「投稿したコメント URL を最終行に
      単独で出力する」を必須化する。抽出の第一候補は `gh pr comment` の stdout
      (= `tool_result` ブロック) — `result` イベント本文 (最終 assistant text) は
      URL を含む保証がないため、抽出元にしない (sidepanel.js `captureCommentUrl`)
- [x] **冪等性 (指摘5)**: コメント冒頭に隠しマーカー `<!-- web-review -->` を入れ、
      テンプレに「投稿前に既存マーカーの有無を確認し、あれば新規投稿せず既存コメントを
      更新 (`gh pr comment --edit-last`) するか skip する」を規約化。再走・リトライでの
      重複投稿を防ぐ

### 権限 (`--allowedTools`) — 最小権限 (指摘1, 6)

- [x] **read-only を担保する最小 allowlist を確定する。`Bash(gh api *)` /
      `Bash(gh pr *)` の丸ごと許可は禁止** — `gh api` は任意の write API
      (PUT/POST/DELETE)、`gh pr` は `close` / `merge` を通してしまい、自己参照
      レビュー (この draft PR 自体のレビュー) で対象 PR を merge/close しうる。
      確定 (`review.rs review_allowed_tools()`):
      `Bash(gh pr view:*)` / `Bash(gh pr diff:*)` / `Bash(gh pr checks:*)` /
      `Bash(gh pr comment:*)` / `Read`
      (API がどうしても要る場合は GET のみ = `Bash(gh api -X GET *)` を規約化)
- [x] **Edit / Write は付与しない** (レビュー専用。unit test で固定)
- [ ] **ブラウザ系ツールを具体名まで落とす (指摘6)**: `mcp__claude-in-chrome__*`
      のうちレビューに要るもの (navigate / read_page / screenshot / find 系。
      正確なツール名は #1 spike の tool_use サンプル採取で確定) を列挙し、
      拡張側のサイト権限 (github.com / プレビュー URL ドメイン) も明記 — **#1 待ち。
      それまでブラウザ込みレビューは terminal モードで行う (README 明記)**
- [x] terminal モード (#18) では対話承認できるため、まず terminal で観点を検証してから
      `-p` 用 allowlist に落とす (行クリックは terminal 起動中なら terminal へ流し込む)
- [x] 一覧の行クリック時に extraArgs へ自動投入するか、テンプレ側に埋めるかを決める —
      **決定: どちらでもなく `{cmd:"start"}` の独立フィールド `allowed_tools`**。rule は
      空白を含むため extraArgs (空白 split) に載らず、host が `--allowedTools` の
      1 argv (comma join) に組む

### PR コメント投稿経路

- [x] `gh` CLI (認証済み前提を README に明記) か githubmcp かを決定 — **決定: `gh` CLI**
      (allowlist を `gh pr` サブコマンド粒度で絞れる。README に `gh auth login` 前提を明記)
- [x] **コメント URL の抽出は `tool_result` (`gh pr comment` の stdout) を第一候補**、
      テンプレの「URL 単独行出力」規約をフォールバックにする (指摘2)。抽出できたら
      完了カードにリンク表示

### 失敗時の導線

- [x] コメント未投稿で終了した場合の「続きから」(`--resume <session_id>`) 起動ボタン
      (`{cmd:"resume"}` — session_id は host が `last_session.json` から解決。
      URL 未検出で終了した review 実行には警告 + 再開導線を表示)
- [x] **resume の取り違え防止 (指摘3)**: `last_session.json` はグローバル 1 本のため、
      複数 PR を続けて回すと直近の別 PR セッションを resume しうる。対応を二択で決める:
      (a) `last_session.json` を `{ pr_key: session_id }` の map に拡張して per-PR resume、
      (b) 当面は「resume は直近 1 件のみ」を UI 文言 (ボタン title / status) で明示。
      **MVP は (b) で実装済み**、#5 の後続で (a)

### CCoW への引き継ぎトリガ (指摘4)

- [x] **CCoW がレビューコメントをどう検知して起動するかを確定する** (#7 の依存として整理):
      - 案 A (推奨・実績あり): CCoW セッションが対象 PR を `subscribe_pr_activity` で
        watch し、`<!-- web-review -->` マーカー付きコメントの webhook で起床して
        「CCoW への引き継ぎ」チェックリストを処理する — この経路は本 draft PR #24 の
        レビューコメントで実際に一周することを確認済み (2026-07-09)
      - 案 B: github-mcp-rs の `subscribe_issue_activity` + `get_pending_events` polling
      - 案 C: 手動 (コメント URL を CCoW に貼る) — フォールバックとして常に可能

      **決定: 案 A (subscribe_pr_activity webhook 起床)、フォールバックは案 C**
- [x] 引き継ぎコメントの機械可読部 (マーカー + チェックリスト形式) をテンプレに固定する
      (`<!-- web-review -->` 1 行目 + `## CCoW への引き継ぎ` チェックリスト)

## 受け入れ条件 (issue #5 より)

一覧から選んだ draft PR に対し、無人でレビューコメントが付き、CCoW 側でそのコメントを
起点に作業を引き継げる。

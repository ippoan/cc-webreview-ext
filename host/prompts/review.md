<!--
  レビュープロンプトテンプレート (cc-webreview-ext#5)。
  cc-webreview-agent が include_str! でバイナリに同梱し、{cmd:"review_prompt"} 受信時に
  プレースホルダを差し込んで返す (host/agent/src/review.rs)。この先頭コメントは
  render 時に取り除かれ、claude には渡らない。
  プレースホルダ: {{PR_URL}} {{PR_REPO}} {{PR_NUMBER}} {{PR_TITLE}} {{PR_AUTHOR}}
-->
draft PR {{PR_REPO}}#{{PR_NUMBER}} のレビューを実施し、結果を PR コメントとして投稿する。
**PR コメントの投稿までがこのタスクの完了条件** (レビューして応答本文に書くだけでは未完了)。

対象:

- PR: {{PR_URL}}
- タイトル: {{PR_TITLE}}
- 作成者: {{PR_AUTHOR}}

## レビュー手順

1. `gh pr view {{PR_NUMBER}} --repo {{PR_REPO}}` で説明・目的を確認する
2. `gh pr diff {{PR_NUMBER}} --repo {{PR_REPO}}` で差分を確認する
3. `gh pr checks {{PR_NUMBER}} --repo {{PR_REPO}}` で CI 状態を確認する
4. (ブラウザツールが使える場合のみ) PR ページやプレビュー URL を開いて表示・動作を確認する
5. 指摘は 正しさ → 設計 → 可読性 の順で重大度を付けて整理する

## PR コメント投稿 (必須)

投稿前に `gh pr view {{PR_NUMBER}} --repo {{PR_REPO}} --comments` で既存コメントを確認し、
次の 2 分岐で必ず投稿する (skip はしない — skip するとレビュー結果がどこにも残らない):

- **この PR での自分の最後のコメント**が `<!-- web-review -->` マーカー付きの場合のみ
  `--edit-last` でそのコメントを更新する (同一ラウンドの再走・リトライで重複投稿しない)。
  **`--edit-last` はマーカーに関係なく「自分の最後のコメント」を編集する**ため、
  この条件を満たさない時に使ってはいけない
- それ以外 (マーカー付きコメントが無い / 自分のマーカー付きコメントより後に別の
  コメントが続いている / マーカーが他人の投稿) は**新規投稿**する —
  対応後にレビューし直した新しいラウンドの結果は、新規コメントとして積むのが正

**新規投稿は、`mcp__githubmcp__add_issue_comment` または `mcp__github__add_issue_comment`
tool が使える環境ではそれを優先する** (GitHub App token 経由のため、gh CLI (user PAT) の
GraphQL rate limit と別枠 — PAT が枯渇していても投稿できる)。issue_number には PR 番号
{{PR_NUMBER}} をそのまま渡す。

MCP tool が無い場合は次の 1 コマンド形式で投稿する (本文は stdin 渡し。一時ファイルの
作成 (`Write` / `cat >` / PowerShell 等) は許可されていないため試みない):

```sh
gh pr comment {{PR_NUMBER}} --repo {{PR_REPO}} --body-file - <<'WEBREVIEW_EOF'
<!-- web-review -->
## Web Review 結果
…
WEBREVIEW_EOF
```

更新 (`--edit-last` の分岐) は MCP tool では行えないため常に gh の同形式に
`--edit-last` を足して行う。本文に `WEBREVIEW_EOF` という行を含めないこと。

投稿が rate limit 等の一時エラーで失敗した場合は、60 秒ほど待って **1 回だけ**
再試行する。それでも失敗したら諦めて `failed: <理由>` で終了する (リトライを
繰り返さない — 「続きから」で後から再開できる)。

コメント本文は次の書式に従う (1 行目の `<!-- web-review -->` マーカー必須):

```
<!-- web-review -->
## Web Review 結果

- (重大度) 指摘 1 …
- (重大度) 指摘 2 …

## CCoW への引き継ぎ

- [ ] 対応タスク 1 …
- [ ] 対応タスク 2 …
```

- 指摘が無ければ「指摘なし」と書き、「CCoW への引き継ぎ」チェックリストは省略してよい
- issue を参照する時は `Refs #N` を使う (`Closes` / `Fixes` / `Resolves` は禁止 —
  auto-close させない)

## 出力規約

全て終えたら、投稿 (または更新) したコメントの URL を応答の**最終行に URL 単独の行**として
出力する。投稿・更新に失敗した場合は URL の代わりに `failed: <理由>` を最終行に出力する。

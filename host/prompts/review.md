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

投稿前に `gh pr view {{PR_NUMBER}} --repo {{PR_REPO}} --comments` で既存コメントを確認する:

- `<!-- web-review -->` マーカー付きコメントが**自分の最後の投稿**として既にある場合は、
  新規投稿せず `gh pr comment {{PR_NUMBER}} --repo {{PR_REPO}} --edit-last --body "..."` で
  そのコメントを更新する (再走・リトライで同内容を重複投稿しない)
- マーカー付きコメントが他人の投稿の場合は投稿を skip し、その旨を報告する
- 無ければ `gh pr comment {{PR_NUMBER}} --repo {{PR_REPO}} --body "..."` で新規投稿する

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
出力する。投稿を skip した場合は URL の代わりに `skipped: <理由>` を最終行に出力する。

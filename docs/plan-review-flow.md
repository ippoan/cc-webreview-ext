# Plan: レビューフロー統合 (issue #5)

Refs #5 / Part of #7 — **draft**: 実装前の設計下書き。PR レビューで内容を確定してから
着手する (このファイル自体が #4 の draft PR 一覧の実データ確認も兼ねる)。

## ゴール

side panel の draft PR 一覧 (#4、実装済み) から 1 クリックで「ブラウザ込みレビュー →
PR コメント投稿 → CCoW 引き継ぎ」まで無人で走る。

## チェックリスト

### プロンプトテンプレート

- [ ] `host/prompts/review.md` としてテンプレートを repo 管理する (sidepanel.js の
      `reviewPrompt()` ハードコードから移す。host が読み込んで差し込むか、拡張に
      同梱するかは要検討 — 拡張同梱なら zip / wix への追加も)
- [ ] 差し込み変数: PR URL / repo / number / title / author
- [ ] レビュー観点: diff、CI 状態、必要ならブラウザで PR ページ・プレビュー URL を目視
- [ ] 出力の必須タスク化: **PR コメント投稿まで**を完了条件として明記する
- [ ] コメント書式: `## Web Review 結果` + 指摘リスト + `## CCoW への引き継ぎ`
      (対応タスクのチェックリスト)。`Refs #N` 規約 (auto-close 禁止) も明記

### 権限 (`--allowedTools`)

- [ ] レビュー専用の allowlist を確定: browser 系 + `Bash(gh pr *)` + `Bash(gh api *)`
      + Read。**Edit / Write は付与しない**
- [ ] terminal モード (#18) では対話承認できるため、まず terminal で観点を検証してから
      `-p` 用 allowlist に落とす
- [ ] 一覧の行クリック時に extraArgs へ自動投入するか、テンプレ側に埋めるかを決める

### PR コメント投稿経路

- [ ] `gh` CLI (認証済み前提を README に明記) か githubmcp かを決定
- [ ] `result` イベントからコメント URL を抽出し、完了カードにリンク表示

### 失敗時の導線

- [ ] コメント未投稿で終了した場合の「続きから」(`--resume <session_id>`) 起動ボタン
      (host は last_session.json に session_id を永続化済み)

## 受け入れ条件 (issue #5 より)

一覧から選んだ draft PR に対し、無人でレビューコメントが付き、CCoW 側でそのコメントを
起点に作業を引き継げる。

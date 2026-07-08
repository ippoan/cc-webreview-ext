# Spike: claude --chrome × native host headless spawn の検証記録

Refs #1, #3, #11 — 検証環境: Windows ネイティブ claude.exe + agent-dev-6 (MSI)。

## 確定したこと (2026-07-08)

### native host からの headless spawn は成立する

`cc-webreview-agent` (Chrome native messaging host) から
`claude -p <prompt> --output-format stream-json --verbose --chrome` を spawn し、
stream-json (JSONL) を side panel に中継する縦切りが実機で完走した。

- 観測イベント列: `system` (init, session_id あり) → `rate_limit_event` →
  `assistant` → `result` (result 本文 / session_id / total_cost_usd / num_turns)
- MSI 配布 (git clone 不要) + install 時 custom action の `--register` で
  そのまま繋がる。Chrome 再起動も不要だった

### 認証: CLAUDE_CODE_OAUTH_TOKEN (ユーザー環境変数) で通る

Chrome → native host → claude の環境変数継承で認証が通ることを確認 (#11)。

- 手順: `claude setup-token` → `setx CLAUDE_CODE_OAUTH_TOKEN "<token>"` →
  `taskkill /IM chrome.exe /F` → Chrome 起動し直し
- host 側の env 注入機能は**不要** (ユーザー環境変数は GUI 起動の Chrome にも継承される)
- 罠 1: 旧バージョンの claude は未ログインでも `401 Invalid authentication
  credentials` を返す (紛らわしい)。まず claude を最新化してから切り分ける
- 罠 2: `setx` は既存プロセスに効かない。既存ターミナルは開き直す。Chrome は
  完全終了 (`taskkill`) してから起動し直す

## 未確定 (次の検証)

- [ ] **`--chrome` で browser ツールが headless (-p) で実際に使えるか** — 今回の
  応答はブラウザ操作を伴わないプロンプトだったため未確認。検証プロンプト例:
  「https://github.com/ippoan/cc-webreview-ext/pulls をブラウザで開いて、見えている
  PR のタイトルを列挙して」。side panel のタイムラインに browser 系 tool_use が
  出るか / 実タブが動くかを観る
- [ ] `-p` で browser 不可だった場合: `--input-format stream-json` 双方向セッションでの
  可否 (#3 の stdin 中継を実装してから)
- [ ] browser tool_use イベントの形 (tool 名・入力) のサンプル採取 → 本 doc に追記
- [ ] `--allowedTools` で browser 系 + `Bash(gh *)` + Read に絞った時の権限プロンプト挙動
- [ ] 多重セッション時の named pipe 競合 (host 側は busy 排他済み。拡張側の
  Claude in Chrome 公式連携との競合を確認)
- [ ] 拡張 service worker idle 時の切断 → 再接続挙動

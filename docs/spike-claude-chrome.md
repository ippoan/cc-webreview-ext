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

### browser ツールは MCP server `claude-in-chrome` として生える (2026-07-09、公式 docs)

公式 docs (<https://code.claude.com/docs/en/chrome>) より:

- browser ツールの完全一覧は docs に無く「Run `/mcp` and select **`claude-in-chrome`**
  to see the full list of available browser tools」と案内される = **MCP server 名は
  `claude-in-chrome`**。permission rule は server 単位の `mcp__claude-in-chrome` で
  全ツールを許可できる (個別名は版で増減するため server rule を採用、#31)
- read 系 (`read_page` / `get_page_text` / `find` / console / network / screenshot) は
  permission prompt 無し、状態変更系 (click / type / navigation / tab 管理) は prompt
  対象 — headless では allowlist に入っていなければ deny される
- headless (`-p`) での `--chrome` サポートは **docs に記載が無い** (下の未確定項目のまま)。
  #31 の実装は「provision されなければ allowlist rule が使われないだけ」の fail-safe

### headless (-p) で `--chrome` の browser ツールが使えることを実機確定 (2026-07-09、#31 接続プローブ)

agent-dev-53 の「接続プローブ」(claude 2.1.205、`--allowedTools mcp__claude-in-chrome`) で確定:

- **`-p` (headless) でも `claude-in-chrome` MCP server は provision される**。init event の
  `mcp_servers` に `{"name":"claude-in-chrome","status":"pending"}` が出る
- **ツールは deferred tools (54 個) として生え、モデルが `ToolSearch`
  (`select:mcp__claude-in-chrome__<tool>`) でロードしてから呼ぶ**。allowlist の
  server rule `mcp__claude-in-chrome` で呼び出しは通った (permission denial なし)
- tool_use サンプル: `mcp__claude-in-chrome__tabs_context_mcp`、input `{"createIfEmpty": false}`。
  side panel には `tool_use_meta.server_display_name: "Claude in Chrome"` 付きで流れる
- 未解決はツール実行時の **`Browser extension is not connected`** のみ (cloud relay が
  公式拡張を見つけられない)。原因候補: 公式拡張が対象プロファイルで未サインイン /
  Claude Code 側アカウント (env `CLAUDE_CODE_OAUTH_TOKEN` が credentials より優先) と
  拡張のログインアカウントの不一致 / 複数マシンの拡張が relay を取っている
- 運用メモ: **プローブ実行中に side panel を閉じる/リロードすると port closed →
  claude kill** (既定の同時 1 本規約どおり)。拡張 self-update 直後はリロードを挟むため
  プローブは更新完了後に実行する

### `Browser extension is not connected` の実解決記録 (2026-07-09)

半日の切り分けの結論: **原因は拡張側ではなく CLI 側の認証セッション**。`/login` の
やり直しで復活した。将来の再発時は下の順で確認する:

1. 症状の確定: `/chrome` → `Select browser…` が空 (または `Couldn't list connected
   browsers: JSON Parse error: Unexpected identifier "Browser"` — relay が text/plain
   のエラー本文を返し CLI が JSON parse に失敗する表示バグ。意味は「このアカウントに
   接続中の拡張ゼロ」)
2. 棄却できた仮説 (再発時に再調査しない): **cwd/フォルダは無関係** (work dir でも
   %USERPROFILE% でも同じ結果)。**アカウント不一致でもなかった** (CLI・拡張とも同一
   email で再現)。Anthropic status も operational だった
3. **解決: CLI で `/login` をやり直す** — 認証セッションの relay 紐付けが失効していると
   拡張がログイン済みでも connected browsers がゼロになる
4. 罠: `/login` が直すのは credentials file。**env `CLAUDE_CODE_OAUTH_TOKEN` が残って
   いると headless (cc-webreview) はそちらを優先**するため、古い token のままだと
   プローブは FAIL し続ける。env token を廃止して credentials に一本化するか、
   `claude setup-token` で取り直して `setx` し直す (どちらも Chrome 完全再起動が必要)

## 未確定 (次の検証)

- [x] ~~`--chrome` で browser ツールが headless (-p) で実際に使えるか~~ → **使える** (上記)
- [x] ~~browser tool_use イベントの形 (tool 名・入力) のサンプル採取~~ → 上記に追記
- [x] ~~`Browser extension is not connected` の解消~~ → **解決** (上記、CLI 側 `/login`)
- [ ] headless プローブの PASS 確認 (env token 差し替え/廃止後) → nuxt-dtako-admin#200 の
  `--chrome` レビュー再走で pr-chat-bridge:result 投稿まで end-to-end 確認
- [ ] `--allowedTools` で browser 系 + `Bash(gh *)` + Read に絞った時の権限プロンプト挙動
- [ ] 多重セッション時の named pipe 競合 (host 側は busy 排他済み。拡張側の
  Claude in Chrome 公式連携との競合を確認)
- [ ] 拡張 service worker idle 時の切断 → 再接続挙動

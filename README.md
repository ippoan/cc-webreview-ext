# cc-webreview-ext

draft PR の「ブラウザ込みレビュー」を Chrome の side panel から起動するツール。

```
Side Panel ──▶ Service Worker ──connectNative──▶ cc-webreview-agent (Rust)
    ▲                                                  │ spawn
    │  stream-json (JSONL) を中継・描画                 ▼
    └──────────────────────────────── claude -p --output-format stream-json [--chrome]
```

- `extension/` — MV3 side panel 拡張 (素 JS)
- `host/` — Rust native messaging host (`cc-webreview-agent`)

全体設計は [tracking issue #7](https://github.com/ippoan/cc-webreview-ext/issues/7)。

## Windows セットアップ (MSI、git clone 不要)

前提: Windows ネイティブ claude.exe (v2.0.73+、WSL 不可)。`--chrome` を試す場合は
Claude in Chrome 拡張 v1.0.36+ も。

1. **MSI インストール**: [Releases](https://github.com/ippoan/cc-webreview-ext/releases)
   から `cc-webreview-agent-*-x86_64.msi` を実行 (perUser install、admin 不要)。
   host exe + 拡張が `%LOCALAPPDATA%\Programs\cc-webreview-agent\` に入り、
   native host の HKCU 登録も install 時に自動で行われる。

2. **拡張をロード**: `chrome://extensions` → デベロッパーモード ON →
   「パッケージ化されていない拡張機能を読み込む」→
   `%LOCALAPPDATA%\Programs\cc-webreview-agent\extension` を選択。
   ID が `hkinllfgncahghgkimjjcdppgnglijcb` になることを確認
   (manifest の `key` で固定済み。違う ID になったら manifest が壊れている)。

3. **claude.exe のパス設定 (必要な場合のみ)**: `%USERPROFILE%\.local\bin\claude.exe`
   に居るなら不要。それ以外の場所なら一度だけ (実パスを自動解決してそのまま登録):

   ```powershell
   $claude = (Get-Command claude.exe -ErrorAction Stop).Source
   & "$env:LOCALAPPDATA\Programs\cc-webreview-agent\cc-webreview-agent.exe" --register --claude-path $claude
   ```

   実在しないパスを渡すと登録は失敗する (プレースホルダのコピペ事故防止)。
   `Get-Command` で見つからない場合は `where.exe claude` で場所を探す。

4. **動作確認**: ツールバーの cc-webreview アイコン (または `Alt+C` — 開閉トグル、
   変更は `chrome://extensions/shortcuts`) → side panel が開く →
   `Ping` で `host vX.Y.Z 接続 / claude: <path>` が出れば host 接続 OK →
   prompt を入れて `Start`。claude の stream-json イベントがタイムラインに流れる。
   service worker の console (`chrome://extensions` → Service Worker「検証」) にも全イベントが出る。

### リリースサイクル

- **dev**: PR (non-draft) の CI が build 1 回で test → MSI → `agent-dev-<run>` の
  prerelease 添付まで行う (`ci.yml` の dev-release job → `release.yml` を workflow_call)。
  **merge を待たずに PR 時点の MSI を試せる**。auto-merge は MSI ビルド成功も gate。
- **stable**: `agent-vX.Y.Z` tag push (非 prerelease = Latest)。
- 手動更新は新しい MSI を入れ直すだけ (MajorUpgrade で上書き、native host 登録も貼り替え)。

### 自動更新 (self-update)

リリースビルド (tag 埋め込みあり) の agent は、Chrome から起動されるたびに
バックグラウンドで GitHub Releases をチェックし:

- **agent 本体**: `agent-dev-N` の新しい release があれば
  `cc-webreview-agent-<tag>-x86_64-pc-windows-msvc.zip` を DL → **minisign 署名検証**
  (`.minisig`、公開鍵は update.rs にハードコード) → `self_replace`。**次回起動から反映**。
- **同梱拡張**: install dir の `extension\` を最新の `cc-webreview-extension-<tag>.zip`
  (こちらも署名検証必須) で上書きし、side panel に「リロードで反映」を通知
  (`.ext-version` marker で適用済み tag を記録)。

ローカルビルド (tag なし) は自動更新しない (開発中の自分を上書きしない)。
署名の秘密鍵は org secret `MINISIGN_SECRET_KEY` (cdp-relay と共用)、release workflow
(`publish` job) が全 zip asset を署名する。署名の無い asset は適用されない。

### ソースからビルドする場合 (開発者向け)

```powershell
git clone https://github.com/ippoan/cc-webreview-ext
cd cc-webreview-ext\host
cargo build --release
.\target\release\cc-webreview-agent.exe --register --claude-path "C:\Users\<you>\.local\bin\claude.exe"
```

拡張はリポジトリの `extension/` を unpacked ロードする。

## トラブルシュート

| 症状 | 対処 |
|---|---|
| `Specified native messaging host not found` | `--register` をやり直す。拡張 ID が固定 ID と一致しているか確認 |
| `claude が見つからない` | `--register --claude-path <絶対パス>` で再設定 (PATH は見ない仕様) |
| `401 Invalid authentication credentials` / `Not logged in` | まず claude を最新化 (旧版は未ログインでも 401 を出す)。`/login` が通らない場合は `claude setup-token` → `setx CLAUDE_CODE_OAUTH_TOKEN "<token>"` → `taskkill /IM chrome.exe /F` → Chrome 起動し直し (詳細: [docs/spike-claude-chrome.md](./docs/spike-claude-chrome.md)、経緯: #11) |
| 何が起きたか分からない | `cc-webreview-agent.exe --debug-dump 200` — host を通った全イベント (制御 msg / stream-json / stderr / proc) が `%LOCALAPPDATA%\cc-webreview\debug.sqlite` に残っている。sqlite3 で直接 `SELECT * FROM events` も可 |
| busy と言われる | 既に claude セッションが走っている。`Stop` してから再実行 |

## ターミナルモード

`-p` (headless) は権限承認プロンプトに応答できない。side panel の **Terminal** ボタンは
対話モードの claude を PTY (Windows: ConPTY) 配下で起動し、xterm.js (同梱 vendor) に
生画面を流す — 承認プロンプト・`/login`・`/chrome` がそのまま使える。

- `--chrome` checkbox / 追加 CLI 引数は Terminal 起動にも効く
- -p セッションと terminal は**同時 1 本** (`busy` で拒否)
- panel を閉じる / `Term 終了` で claude は kill される (ゾンビを残さない)

## レビューフロー (#5)

draft PR 一覧 (ci-dashboard webhook-fed) の行をクリックすると、host が repo 管理の
テンプレ [`host/prompts/review.md`](./host/prompts/review.md) (バイナリに同梱、更新は
agent self-update に相乗り) に PR 情報を差し込んで返し、prompt 欄 (terminal 起動中は
claude の入力欄) に入る。`Start` で `-p` レビューが走り、**PR コメント投稿までを完了
条件**とする。投稿コメントの URL は `gh pr comment` の stdout (`tool_result`) から抽出
して完了カードにリンク表示する (フォールバック: result 最終行の URL 単独行)。

- **前提: `gh` CLI がインストール済みかつ認証済み (`gh auth login`) であること**。
  コメント投稿は claude が `gh pr comment` で行う
- `-p` 実行には read-only の最小 allowlist
  (`gh pr view/diff/checks/comment` + `Read`) が自動適用される。`gh api` / `gh pr` の
  丸ごと許可はしない (merge / close / 任意 write API を通さない)。Edit / Write なし
- 再走しても重複投稿しない: コメント冒頭の `<!-- web-review -->` マーカーを確認し、
  既存があれば `--edit-last` で更新する (テンプレで規約化)
- コメント未投稿で終了した場合は「**続きから**」ボタンで直近セッションを
  `--resume` 再開できる。**resume は直近 1 件のみ** (記録がグローバル 1 本のため、
  複数 PR を続けて回した場合は最後のセッションだけ)
- ブラウザ系ツールの allowlist 追加は #1 spike (tool_use ツール名の採取) 後。それまで
  ブラウザ込みレビューは Terminal モード (対話承認) で行う

CCoW への引き継ぎ: コメントの `## CCoW への引き継ぎ` チェックリストを、対象 PR を
`subscribe_pr_activity` で watch している CCoW セッションが webhook 起床で処理する
(検証済みの経路 — docs/plan-review-flow.md 参照)。

## プロトコル (拡張 ↔ host)

- 拡張 → host: `{cmd:"start", prompt, chrome?, extra_args?, cwd?, allowed_tools?}` / `{cmd:"resume", prompt, chrome?, extra_args?, cwd?, allowed_tools?}` / `{cmd:"review_prompt", pr:{repo, number, url, title, author}}` / `{cmd:"stop"}` / `{cmd:"ping"}` / `{cmd:"check_update"}` / `{cmd:"term_start", cols, rows, chrome?, extra_args?, cwd?}` / `{cmd:"term_input", data}` / `{cmd:"term_resize", cols, rows}` / `{cmd:"term_kill"}` / `{cmd:"debug_dump", limit?}`
- `allowed_tools` は permission rule の配列 (`Bash(gh pr view:*)` 等)。rule は空白を
  含むため extra_args (空白 split) では運べず、host が `--allowedTools` の 1 引数
  (comma join) に組む。`resume` の session_id は host が `last_session.json` から解決
- **prompt は claude の stdin に渡す** (argv 渡しは改行入り prompt が `cmd /C` で分断され、
  `-` 始まりの行が `unknown option` になるため禁止)
- host → 拡張: `{type:"hello"|"pong"|"claude"|"raw"|"stderr"|"proc"|"busy"|"error"|"update"|"update_status"|"term_out"|"term_exit"|"debug_dump"|"review_prompt"}`。
  `term_out.data` は base64 (PTY チャンクは UTF-8 多バイト文字を分断し得るため)。
  512KB 超は `{type:"chunk", id, seq, last, data}` に分割 (background.js が再結合)。

## 開発

```sh
cd host
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
```

ロジックは CCoW (Linux) で unit test、実機検証 (registry / spawn / 拡張接続) は Windows のみ。

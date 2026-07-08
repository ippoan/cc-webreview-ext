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

## Windows セットアップ (検証手順)

前提: Windows ネイティブ claude.exe (v2.0.73+、WSL 不可)。`--chrome` を試す場合は
Claude in Chrome 拡張 v1.0.36+ も。

1. **ビルド**

   ```powershell
   git clone https://github.com/ippoan/cc-webreview-ext
   cd cc-webreview-ext\host
   cargo build --release
   ```

2. **拡張をロード**: `chrome://extensions` → デベロッパーモード ON →
   「パッケージ化されていない拡張機能を読み込む」→ `extension/` フォルダを選択。
   ID が `hkinllfgncahghgkimjjcdppgnglijcb` になることを確認
   (manifest の `key` で固定済み。違う ID になったら manifest が壊れている)。

3. **native host 登録** (claude.exe の場所は `where claude` などで確認):

   ```powershell
   .\target\release\cc-webreview-agent.exe --register --claude-path "C:\Users\<you>\.local\bin\claude.exe"
   ```

   HKCU (Chrome / Edge) に登録されるので admin 不要。Chrome の再起動も原則不要。

4. **動作確認**: ツールバーの cc-webreview アイコン → side panel が開く →
   `Ping` で `host vX.Y.Z 接続 / claude: <path>` が出れば host 接続 OK →
   prompt を入れて `Start`。claude の stream-json イベントがタイムラインに流れる。
   service worker の console (`chrome://extensions` → Service Worker「検証」) にも全イベントが出る。

## トラブルシュート

| 症状 | 対処 |
|---|---|
| `Specified native messaging host not found` | `--register` をやり直す。拡張 ID が固定 ID と一致しているか確認 |
| `claude が見つからない` | `--register --claude-path <絶対パス>` で再設定 (PATH は見ない仕様) |
| 何が起きたか分からない | `cc-webreview-agent.exe --debug-dump 200` — host を通った全イベント (制御 msg / stream-json / stderr / proc) が `%LOCALAPPDATA%\cc-webreview\debug.sqlite` に残っている。sqlite3 で直接 `SELECT * FROM events` も可 |
| busy と言われる | 既に claude セッションが走っている。`Stop` してから再実行 |

## プロトコル (拡張 ↔ host)

- 拡張 → host: `{cmd:"start", prompt, chrome?, extra_args?, cwd?}` / `{cmd:"stop"}` / `{cmd:"ping"}`
- host → 拡張: `{type:"hello"|"pong"|"claude"|"raw"|"stderr"|"proc"|"busy"|"error"}`。
  512KB 超は `{type:"chunk", id, seq, last, data}` に分割 (background.js が再結合)。

## 開発

```sh
cd host
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
```

ロジックは CCoW (Linux) で unit test、実機検証 (registry / spawn / 拡張接続) は Windows のみ。

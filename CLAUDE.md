# cc-webreview-ext

draft PR のブラウザ込みレビューを回す **MV3 side panel 拡張 + Rust native host**。
拡張 → native host (`cc-webreview-agent`) → `claude -p --output-format stream-json` を
spawn し、JSONL を side panel に中継する。全体像は tracking issue #7、cdp-relay の
nmhost/register 実装を流用 (SOURCE-MIRROR 宣言済み)。

## ビルド / テスト

```sh
cd host
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

ロジック (framing / chunk 分割 / コマンド parse / debug log) は CCoW (Linux) で
unit test する。registry 登録・実 claude spawn・拡張との接続の検証は **Windows 実機のみ**。

## 不変則 (必ず守る)

- **native host の stdout は framed JSON 専用**。println! 等を混ぜない (log は stderr +
  debug.sqlite)。
- **claude のパスは絶対パスで解決** (registry `ClaudeExe` / env `CC_WEBREVIEW_CLAUDE`)。
  Chrome 起動の host は環境変数が最小限なので PATH 依存禁止。
- **host → Chrome は 1 message 1MB 上限** — `{type:"chunk"}` 分割プロトコルを崩さない。
- **同時 claude セッションは 1 本** (`busy` で拒否)。port 切断時は kill してゾンビを残さない。
- 拡張 ID は manifest.json の `key` で固定 (`hkinllfgncahghgkimjjcdppgnglijcb`)。
  `key` を変えると native host の allowed_origins と不整合になる。

## GitHub 自動化

- **`main` に直 push しない。** PR を作る。
- PR / commit は `Refs #N` (`Closes/Fixes/Resolves` 禁止 — auto-close 防止)。
- PR 作成後は同じ turn で `mcp__github__subscribe_pr_activity` を呼び CI を watch する。

---

_共通項を直すときは [`ippoan/claude-md`](https://github.com/ippoan/claude-md) の
`CLAUDE.md.template` を更新すること。_

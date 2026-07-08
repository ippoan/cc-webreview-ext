// cc-webreview side panel (cc-webreview-ext#3, #4 の仮 UI)
// service worker と Port で繋ぎ、host イベントをタイムライン描画する。
// 開いた時に replay を要求して進行中セッションへ再アタッチする。

const statusEl = document.getElementById('status');
const timeline = document.getElementById('timeline');
const promptEl = document.getElementById('prompt');
const chromeEl = document.getElementById('chrome');
const authBanner = document.getElementById('authBanner');
const authReason = document.getElementById('authReason');

const extraArgsEl = document.getElementById('extraArgs');

const port = chrome.runtime.connect({ name: 'panel' });
port.onMessage.addListener(render);
port.postMessage({ cmd: 'replay' });

document.getElementById('start').addEventListener('click', () => {
  const prompt = promptEl.value.trim();
  if (!prompt) {
    setStatus('prompt を入力してください');
    return;
  }
  timeline.textContent = '';
  lastAssistantText = '';
  // 再実行時はバナーを一旦引っ込め、新セッションで再検知させる。
  authBannerSticky = false;
  authBanner.hidden = true;
  // 追加 CLI 引数 (空白区切り)。-p は対話承認できないため --allowedTools 等を渡す用。
  const extraArgs = extraArgsEl.value.trim() ? extraArgsEl.value.trim().split(/\s+/) : [];
  port.postMessage({ cmd: 'start', prompt, chrome: chromeEl.checked, extra_args: extraArgs });
  setStatus('起動中…');
});
document.getElementById('stop').addEventListener('click', () => port.postMessage({ cmd: 'stop' }));
document.getElementById('ping').addEventListener('click', () => port.postMessage({ cmd: 'ping' }));
document.getElementById('checkUpdate').addEventListener('click', () => {
  port.postMessage({ cmd: 'check_update' });
  setStatus('更新確認中…');
});

function setStatus(text) {
  statusEl.textContent = text;
}

// --- login 導線 (#13) ---------------------------------------------------
// 未ログインらしき状態を検知したら setup-token 手順へのバナーを出す。
// - host の hello/pong `auth` (boolean のみ) → 認証情報が見つからない時に表示
// - result / stderr の認証エラー文言 → sticky 表示 (credentials があっても壊れて
//   いる場合があるため、次の pong で auth が true でも消さない)

// docs/spike-claude-chrome.md の既知エラー: `401 Invalid authentication credentials`
// / `Not logged in`。素の "401" 単独では反応しない (偽陽性防止)。
const AUTH_ERROR_RE =
  /not logged in|invalid authentication|invalid api key|please run \/login|401 unauthorized/i;

let authBannerSticky = false;

function showAuthBanner(reason, sticky) {
  authReason.textContent = reason;
  authBanner.hidden = false;
  if (sticky) authBannerSticky = true;
}

function hideAuthBannerUnlessSticky() {
  if (!authBannerSticky) authBanner.hidden = true;
}

// hello / pong の auth (boolean のみ) を反映する。auth 無し = 旧 host は何もしない。
function applyAuthStatus(auth) {
  if (!auth) return;
  if (auth.likely_logged_in) {
    hideAuthBannerUnlessSticky();
  } else {
    showAuthBanner(
      '認証情報が見つかりません (credentials.json / CLAUDE_CODE_OAUTH_TOKEN とも未検出)',
      false
    );
  }
}

// hello / pong の status 行に付ける認証ラベル。auth 無し (旧 host) は空。
function authLabel(auth) {
  if (!auth) return '';
  return auth.likely_logged_in ? ' / login: ✓' : ' / login: ✗';
}

// result / stderr のテキストから認証エラーを検知する。
function detectAuthError(text) {
  if (typeof text === 'string' && AUTH_ERROR_RE.test(text)) {
    showAuthBanner('認証エラーを検知しました (未ログインまたは token 失効)', true);
  }
}

function add(cls, text) {
  const div = document.createElement('div');
  div.className = `ev ${cls}`;
  div.textContent = text;
  timeline.appendChild(div);
  div.scrollIntoView({ block: 'nearest' });
  return div;
}

function addCollapsed(summaryText, bodyText) {
  const details = document.createElement('details');
  const summary = document.createElement('summary');
  summary.textContent = summaryText;
  const pre = document.createElement('pre');
  pre.textContent = bodyText;
  details.appendChild(summary);
  details.appendChild(pre);
  timeline.appendChild(details);
}

// tool_use の 1 行サマリ (「Read: src/main.rs」風)。
function toolSummary(block) {
  const input = block.input || {};
  const brief =
    input.file_path || input.path || input.command || input.url || input.prompt || '';
  const briefStr = String(brief).slice(0, 120);
  return `🔧 ${block.name}${briefStr ? ': ' + briefStr : ''}`;
}

// 直近の assistant text (result カードの 2 重表示防止に使う)。
let lastAssistantText = '';

function renderClaudeEvent(data) {
  const t = data.type;
  if (t === 'system' && data.subtype === 'init') {
    setStatus(`session 開始 (${data.session_id || '?'})`);
    return;
  }
  if (t === 'assistant') {
    const content = (data.message && data.message.content) || [];
    for (const block of content) {
      if (block.type === 'text' && block.text) {
        add('ev-text', block.text);
        lastAssistantText = block.text;
      } else if (block.type === 'tool_use') add('ev-tool', toolSummary(block));
    }
    return;
  }
  if (t === 'user') {
    const content = (data.message && data.message.content) || [];
    for (const block of content) {
      if (block.type === 'tool_result') {
        const body =
          typeof block.content === 'string' ? block.content : JSON.stringify(block.content);
        addCollapsed('tool_result', String(body).slice(0, 4000));
      }
    }
    return;
  }
  if (t === 'result') {
    setStatus(`完了 (${data.subtype || 'result'})`);
    const resultText = typeof data.result === 'string' ? data.result : '';
    detectAuthError(resultText);
    // -p の result 本文は最後の assistant text と同一のことが多い。
    // 直前に描画済みなら result カードでは本文を省略する (2 重表示防止)。
    const isDup = resultText.trim() !== '' && resultText.trim() === lastAssistantText.trim();
    const lines = [
      ...(isDup ? [] : [resultText, '']),
      `session_id: ${data.session_id || '?'}`,
      `cost: $${data.total_cost_usd != null ? data.total_cost_usd : '?'} / turns: ${data.num_turns != null ? data.num_turns : '?'}`,
    ];
    add('ev-result', lines.join('\n'));
    return;
  }
  addCollapsed(`event: ${t || '?'}`, JSON.stringify(data, null, 2).slice(0, 4000));
}

function render(msg) {
  if (!msg || !msg.type) return;
  switch (msg.type) {
    case 'hello':
      setStatus(`host v${msg.version} 接続 / claude: ${msg.claude || '未解決'}${authLabel(msg.auth)}`);
      applyAuthStatus(msg.auth);
      break;
    case 'pong':
      setStatus(
        `host v${msg.version} / claude: ${msg.claude || '未解決'} / running: ${msg.running}${authLabel(msg.auth)}`
      );
      applyAuthStatus(msg.auth);
      break;
    case 'claude':
      renderClaudeEvent(msg.data || {});
      break;
    case 'raw':
      detectAuthError(msg.data);
      add('ev-stderr', `raw: ${msg.data}`);
      break;
    case 'stderr':
      detectAuthError(msg.data);
      add('ev-stderr', msg.data);
      break;
    case 'proc':
      if (msg.event === 'exit') {
        setStatus(`claude 終了 (code=${msg.code}) session_id=${msg.session_id || '?'}`);
      }
      add('ev-proc', `proc: ${msg.event}${msg.code != null ? ` code=${msg.code}` : ''}`);
      break;
    case 'update':
      // host 起動時のバックグラウンド更新 (#6)。適用された時だけ届く。
      add(
        'ev-proc',
        msg.component === 'extension'
          ? `拡張を ${msg.tag} に更新しました → chrome://extensions でリロードすると反映されます`
          : `agent を ${msg.tag} に更新しました (次回の起動から反映されます)`
      );
      break;
    case 'update_status': {
      // 「更新確認」ボタンの結果 (最新でもフィードバックを出す)。
      const who = msg.component === 'extension' ? '拡張' : 'agent';
      const text =
        msg.status === 'applied'
          ? msg.component === 'extension'
            ? `拡張を ${msg.tag} に更新しました → chrome://extensions でリロードすると反映されます`
            : `agent を ${msg.tag} に更新しました (次回の起動から反映されます)`
          : msg.status === 'up_to_date'
            ? `${who} は最新です`
            : msg.status === 'dev_build'
              ? 'ローカルビルドのため自動更新の対象外です'
              : `${who} の更新確認に失敗: ${msg.error || '?'}`;
      add(msg.status === 'error' ? 'ev-error' : 'ev-proc', text);
      setStatus('更新確認完了');
      break;
    }
    case 'busy':
      setStatus('busy: 既にセッションが走っています');
      break;
    case 'error':
      add('ev-error', `error: ${msg.error}`);
      setStatus('error');
      break;
    case 'host_disconnected':
      setStatus(`host 切断${msg.error ? `: ${msg.error}` : ''}`);
      add('ev-error', `host 切断${msg.error ? `: ${msg.error}` : ''}`);
      break;
    default:
      addCollapsed(`msg: ${msg.type}`, JSON.stringify(msg, null, 2).slice(0, 4000));
  }
}

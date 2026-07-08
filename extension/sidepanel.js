// cc-webreview side panel (cc-webreview-ext#3, #4 の仮 UI)
// service worker と Port で繋ぎ、host イベントをタイムライン描画する。
// 開いた時に replay を要求して進行中セッションへ再アタッチする。

const statusEl = document.getElementById('status');
const timeline = document.getElementById('timeline');
const promptEl = document.getElementById('prompt');
const chromeEl = document.getElementById('chrome');

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
  port.postMessage({ cmd: 'start', prompt, chrome: chromeEl.checked });
  setStatus('起動中…');
});
document.getElementById('stop').addEventListener('click', () => port.postMessage({ cmd: 'stop' }));
document.getElementById('ping').addEventListener('click', () => port.postMessage({ cmd: 'ping' }));

function setStatus(text) {
  statusEl.textContent = text;
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

function renderClaudeEvent(data) {
  const t = data.type;
  if (t === 'system' && data.subtype === 'init') {
    setStatus(`session 開始 (${data.session_id || '?'})`);
    return;
  }
  if (t === 'assistant') {
    const content = (data.message && data.message.content) || [];
    for (const block of content) {
      if (block.type === 'text' && block.text) add('ev-text', block.text);
      else if (block.type === 'tool_use') add('ev-tool', toolSummary(block));
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
    const lines = [
      data.result || '',
      '',
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
      setStatus(`host v${msg.version} 接続 / claude: ${msg.claude || '未解決'}`);
      break;
    case 'pong':
      setStatus(
        `host v${msg.version} / claude: ${msg.claude || '未解決'} / running: ${msg.running}`
      );
      break;
    case 'claude':
      renderClaudeEvent(msg.data || {});
      break;
    case 'raw':
      add('ev-stderr', `raw: ${msg.data}`);
      break;
    case 'stderr':
      add('ev-stderr', msg.data);
      break;
    case 'proc':
      if (msg.event === 'exit') {
        setStatus(`claude 終了 (code=${msg.code}) session_id=${msg.session_id || '?'}`);
      }
      add('ev-proc', `proc: ${msg.event}${msg.code != null ? ` code=${msg.code}` : ''}`);
      break;
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

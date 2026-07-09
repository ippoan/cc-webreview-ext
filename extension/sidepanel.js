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

// -p 起動の共通前処理 (start / resume)。タイムラインとセッション状態をリセットする。
function beginRun() {
  timeline.textContent = '';
  lastAssistantText = '';
  lastCommentUrl = '';
  lastSystemRow = null;
  lastSystemSubtype = '';
  // 再実行時はバナーを一旦引っ込め、新セッションで再検知させる。
  authBannerSticky = false;
  authBanner.hidden = true;
  // 追加 CLI 引数 (空白区切り)。空白を含む rule (review allowlist) は
  // allowed_tools フィールドで別送する (host が --allowedTools 1 引数に組む)。
  return extraArgsEl.value.trim() ? extraArgsEl.value.trim().split(/\s+/) : [];
}

document.getElementById('start').addEventListener('click', () => {
  const prompt = promptEl.value.trim();
  if (!prompt) {
    setStatus('prompt を入力してください');
    return;
  }
  const extraArgs = beginRun();
  reviewRun = reviewAllowedTools.length > 0;
  port.postMessage({
    cmd: 'start',
    prompt,
    chrome: chromeEl.checked,
    extra_args: extraArgs,
    allowed_tools: reviewAllowedTools,
  });
  setStatus('起動中…');
});
// 「続きから」(#5 失敗時の導線): 直近の -p セッションを --resume で再開する。
// last_session.json はグローバル 1 本 = **resume は直近 1 件のみ** (複数 PR を続けて
// 回した場合、最後に走った別 PR のセッションを掴む — per-PR resume は後続対応)。
document.getElementById('resume').addEventListener('click', () => {
  const extraArgs = beginRun();
  // resume はレビュー専用導線: パネル再読み込みで reviewAllowedTools が空でも
  // review 扱いにする (allowlist は host がレビュー既定を補う。#27 Web Review 指摘)。
  reviewRun = true;
  port.postMessage({
    cmd: 'resume',
    prompt:
      'レビューの続きから再開する。未完了のタスク (特に PR コメント投稿と、最終行への ' +
      'コメント URL 単独行の出力) を完了してください。',
    chrome: chromeEl.checked,
    extra_args: extraArgs,
    allowed_tools: reviewAllowedTools,
  });
  setStatus('直近セッションを resume 中… (resume は直近 1 件のみ)');
});
document.getElementById('stop').addEventListener('click', () => port.postMessage({ cmd: 'stop' }));
document.getElementById('ping').addEventListener('click', () => port.postMessage({ cmd: 'ping' }));
document.getElementById('checkUpdate').addEventListener('click', () => {
  port.postMessage({ cmd: 'check_update' });
  setStatus('更新確認中…');
});
document.getElementById('debugCopy').addEventListener('click', () => {
  port.postMessage({ cmd: 'debug_dump', limit: 50 });
  setStatus('debug ログ取得中…');
});
document.getElementById('clearLog').addEventListener('click', () => {
  // 表示と SW の replay バッファの両方を消す (バッファを残すと panel 再オープンで
  // 全部戻ってくる)。host 側の debug.sqlite はそのまま (debug コピーで参照可能)。
  timeline.textContent = '';
  lastAssistantText = '';
  port.postMessage({ cmd: 'clear_log' });
  setStatus('log をクリアしました');
});

// --- draft PR 一覧 (#4, API: ippoan/ci-dashboard#470) --------------------
// ci-dashboard の webhook-fed 一覧を CF Access cookie 相乗りで fetch する。
// CF Access 未ログイン時は 302 → HTML が返るため、JSON 以外は loud fail
// (cf-access-staging-public-paths の既知の罠 — 黙って空扱いにしない)。

const CI_DASHBOARD = 'https://ci-dashboard.ippoan.org';
const prListEl = document.getElementById('prList');
const prMetaEl = document.getElementById('prMeta');

// レビューフロー (#5) の状態。テンプレは host が repo 管理の host/prompts/review.md を
// 差し込んで返す ({cmd:"review_prompt"} → {type:"review_prompt"})。
// allowlist (read-only gh pr + Read) は応答で受け、start/resume 時に allowed_tools で返す。
let reviewAllowedTools = [];
// この run がレビュー実行か (完了時の「コメント URL 未検出」警告の出し分けに使う)。
let reviewRun = false;
// gh pr comment の stdout (tool_result) から拾ったコメント URL。第一候補は tool_result、
// result 本文の URL 単独行はフォールバック (docs/plan-review-flow.md 指摘2)。
let lastCommentUrl = '';
const COMMENT_URL_RE = /https:\/\/github\.com\/[^\s"'<>\\)]+#issuecomment-\d+/;

function captureCommentUrl(text) {
  if (lastCommentUrl || typeof text !== 'string') return;
  const m = text.match(COMMENT_URL_RE);
  if (m) lastCommentUrl = m[0];
}

// --- 自動レビュー (auto mode, #29) ---------------------------------------
// panel を開いている間、未レビューの draft PR を自動で -p レビューする (主モード)。
// 済み記録は chrome.storage.local (repo#number 単位。head SHA が API に無いため
// push での自動再レビューは対象外 — 再レビューは行クリック)。
// 同時 1 本規約はそのまま: 実行中は積まず、exit 後の一覧再取得で次を拾う。
const autoEl = document.getElementById('autoReview');
let pRunning = false; // -p セッション実行中 (proc spawn/exit で追跡)
let autoPendingKey = ''; // review_prompt 応答待ちの auto 対象 PR ("repo#number")
let reviewedPrs = {}; // { "repo#number": epoch_ms }
let lastPrs = []; // 直近の draft PR 一覧

const prKey = (pr) => `${pr.repo}#${pr.number}`;

chrome.storage.local.get(['autoReview', 'reviewedPrs']).then((v) => {
  autoEl.checked = v.autoReview !== false; // 既定 ON (auto mode を主とする)
  reviewedPrs = v.reviewedPrs || {};
  maybeAutoReview();
});
autoEl.addEventListener('change', () => {
  chrome.storage.local.set({ autoReview: autoEl.checked });
  if (autoEl.checked) loadDraftPrs();
});

function maybeAutoReview() {
  if (!autoEl.checked || termRunning || pRunning || autoPendingKey) return;
  const next = lastPrs.find((pr) => !reviewedPrs[prKey(pr)]);
  if (!next) return;
  autoPendingKey = prKey(next);
  port.postMessage({
    cmd: 'review_prompt',
    pr: {
      repo: next.repo,
      number: next.number,
      url: next.url,
      title: next.title,
      author: next.author,
    },
  });
  setStatus(`自動レビュー: ${autoPendingKey} のテンプレ取得中…`);
}

// auto ON かつ空いている時だけ 60 秒間隔で一覧を再取得する (API を無駄打ちしない。
// 手動の「一覧更新」は従来どおり)。
setInterval(() => {
  if (autoEl.checked && !termRunning && !pRunning && !autoPendingKey) loadDraftPrs();
}, 60_000);

function renderPrList(prs, updatedAt) {
  lastPrs = prs;
  prListEl.textContent = '';
  prMetaEl.textContent = updatedAt ? `更新: ${updatedAt}` : '';
  if (!prs.length) {
    const div = document.createElement('div');
    div.className = 'pr-empty';
    div.textContent = 'レビュー待ちの draft PR はありません';
    prListEl.appendChild(div);
    return;
  }
  for (const pr of prs) {
    const row = document.createElement('div');
    row.className = 'pr-row';
    row.title = `クリックで prompt にレビューテンプレを入れる\n${pr.url}`;
    const ref = document.createElement('span');
    ref.className = 'pr-ref';
    ref.textContent = `${pr.repo}#${pr.number}`;
    const title = document.createElement('span');
    title.className = 'pr-title';
    title.textContent = pr.title;
    const author = document.createElement('span');
    author.className = 'pr-author';
    author.textContent = pr.author;
    row.append(ref, title, author);
    row.addEventListener('click', () => {
      // テンプレ差し込みは host に依頼し、応答 ({type:"review_prompt"}) 側で
      // terminal / prompt 欄へ流し込む。
      port.postMessage({
        cmd: 'review_prompt',
        pr: { repo: pr.repo, number: pr.number, url: pr.url, title: pr.title, author: pr.author },
      });
      setStatus(`${pr.repo}#${pr.number} のレビューテンプレを取得中…`);
    });
    prListEl.appendChild(row);
  }
  maybeAutoReview();
}

async function loadDraftPrs() {
  prMetaEl.textContent = '取得中…';
  try {
    const res = await fetch(`${CI_DASHBOARD}/api/draft-prs`, { credentials: 'include' });
    const ct = res.headers.get('content-type') || '';
    if (!res.ok || !ct.includes('application/json')) {
      throw new Error(
        `draft-prs 取得失敗 (HTTP ${res.status}, ${ct || 'no content-type'}) — ` +
          `CF Access 未ログインの可能性。${CI_DASHBOARD} をブラウザで開いてログインしてから再試行`
      );
    }
    const body = await res.json();
    renderPrList(body.prs || [], body.updatedAt || '');
  } catch (e) {
    prMetaEl.textContent = '';
    prListEl.textContent = '';
    const div = document.createElement('div');
    div.className = 'ev-error';
    div.textContent = String(e && e.message ? e.message : e);
    prListEl.appendChild(div);
  }
}

document.getElementById('prReload').addEventListener('click', loadDraftPrs);
loadDraftPrs(); // panel を開いたら自動で一度取得する

// --- terminal 埋め込み (#18) -------------------------------------------
// 対話モードの claude を PTY (host 側 ConPTY) で動かし、xterm.js に生バイトを流す。
// -p と違い権限承認プロンプトに応答できる。

const termContainer = document.getElementById('term');
const termKillBtn = document.getElementById('termKill');
let term = null;
let fitAddon = null;
// terminal session が host 側で走っているか。終了後の keystroke を host に送って
// error を量産しない (実機で「terminal が起動していない」が連発した対策)。
let termRunning = false;

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function ensureTerm() {
  termContainer.hidden = false;
  termKillBtn.hidden = false;
  if (term) return term;
  term = new Terminal({ fontSize: 12, cursorBlink: true });
  fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(termContainer);
  fitAddon.fit();
  term.onData((d) => {
    if (termRunning) port.postMessage({ cmd: 'term_input', data: d });
  });
  // panel 幅の変化に追従して PTY もリサイズする。
  // panel が畳まれた/隠れた瞬間に fit すると cols=2 等の極小値で PTY を潰して
  // TUI が崩れる (実機 debug dump で term_resize cols=2 を観測)。まともなサイズの
  // 時だけ追従し、極小値は host に送らない。
  new ResizeObserver(() => {
    if (!fitAddon || !termRunning) return;
    if (termContainer.clientWidth < 160 || termContainer.clientHeight < 100) return;
    fitAddon.fit();
    if (term.cols >= 20 && term.rows >= 5) {
      port.postMessage({ cmd: 'term_resize', cols: term.cols, rows: term.rows });
    }
  }).observe(termContainer);
  return term;
}

document.getElementById('termStart').addEventListener('click', () => {
  const t = ensureTerm();
  // 起動中の再クリックは busy エラーにせずフォーカスだけ当てる (再起動したい時は
  // Term 終了 → Terminal)。
  if (termRunning) {
    setStatus('terminal は起動中です — 再起動するには先に「Term 終了」');
    t.focus();
    return;
  }
  t.reset();
  const extraArgs = extraArgsEl.value.trim() ? extraArgsEl.value.trim().split(/\s+/) : [];
  port.postMessage({
    cmd: 'term_start',
    cols: t.cols,
    rows: t.rows,
    chrome: chromeEl.checked,
    extra_args: extraArgs,
  });
  setStatus('terminal 起動中…');
  t.focus();
});
termKillBtn.addEventListener('click', () => port.postMessage({ cmd: 'term_kill' }));

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

// 拡張が更新された時の通知行 (リロードボタン付き)。runtime.reload() は SW ごと
// 再起動するので side panel も一旦閉じる — セッションが走っていない時に押す想定。
function addExtUpdated(tag) {
  const div = document.createElement('div');
  div.className = 'ev ev-proc';
  div.textContent = `拡張を ${tag} に更新しました → `;
  const btn = document.createElement('button');
  btn.textContent = '拡張をリロード';
  btn.title = '拡張をリロードして新版を反映する (side panel は一旦閉じる。実行中セッションがあれば終了する)';
  btn.addEventListener('click', () => chrome.runtime.reload());
  div.appendChild(btn);
  timeline.appendChild(div);
  div.scrollIntoView({ block: 'nearest' });
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

// system イベントの連打 (thinking_tokens 等) で timeline を埋めない:
// 同一 subtype の連続は 1 行をその場で更新する。
let lastSystemRow = null;
let lastSystemSubtype = '';

function addSystemCoalesced(subtype, text) {
  if (lastSystemRow && lastSystemRow.isConnected && lastSystemSubtype === subtype) {
    lastSystemRow.textContent = text;
    return;
  }
  lastSystemRow = add('ev-proc', text);
  lastSystemSubtype = subtype;
}

function renderClaudeEvent(data) {
  const t = data.type;
  if (t === 'system') {
    if (data.subtype === 'init') {
      setStatus(`session 開始 (${data.session_id || '?'})`);
      return;
    }
    // 思考トークンの進捗カウンタ。行として積まず status を上書きするだけにする
    // (大量に届くため、積むと timeline が event: system で埋まり何も見えなくなる)。
    if (data.subtype === 'thinking_tokens') {
      setStatus(`claude 思考中… (~${data.estimated_tokens != null ? data.estimated_tokens : '?'} tokens)`);
      return;
    }
    addSystemCoalesced(data.subtype || '?', `system: ${data.subtype || '?'}`);
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
        // gh pr comment の stdout はコメント URL を出す — 抽出の第一候補 (指摘2)。
        captureCommentUrl(String(body));
        addCollapsed('tool_result', String(body).slice(0, 4000));
      }
    }
    return;
  }
  if (t === 'result') {
    setStatus(`完了 (${data.subtype || 'result'})`);
    const resultText = typeof data.result === 'string' ? data.result : '';
    detectAuthError(resultText);
    // フォールバック: result 本文の URL 単独行 (テンプレの出力規約) からも拾う。
    captureCommentUrl(resultText);
    // -p の result 本文は最後の assistant text と同一のことが多い。
    // 直前に描画済みなら result カードでは本文を省略する (2 重表示防止)。
    const isDup = resultText.trim() !== '' && resultText.trim() === lastAssistantText.trim();
    const lines = [
      ...(isDup ? [] : [resultText, '']),
      `session_id: ${data.session_id || '?'}`,
      `cost: $${data.total_cost_usd != null ? data.total_cost_usd : '?'} / turns: ${data.num_turns != null ? data.num_turns : '?'}`,
    ];
    const card = add('ev-result', lines.join('\n'));
    if (lastCommentUrl) {
      // 完了カードに投稿コメントへのリンクを付ける (指摘2)。
      const p = document.createElement('div');
      const a = document.createElement('a');
      a.href = lastCommentUrl;
      a.target = '_blank';
      a.rel = 'noreferrer';
      a.textContent = `投稿コメント: ${lastCommentUrl}`;
      p.appendChild(a);
      card.appendChild(p);
    } else if (reviewRun) {
      add(
        'ev-error',
        'コメント URL を検出できませんでした — 未投稿の可能性があります。' +
          '「続きから」で直近セッションを再開できます (resume は直近 1 件のみ)'
      );
    }
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
    case 'review_prompt': {
      // host が差し込んだレビューテンプレ (#5)。terminal 起動中は claude の入力欄へ、
      // それ以外は prompt 欄へ入れる。allowlist は次の Start / 続きから で自動適用。
      const p = msg.prompt || '';
      reviewAllowedTools = msg.allowed_tools || [];
      const ref = msg.pr ? `${msg.pr.repo}#${msg.pr.number}` : '';
      if (autoPendingKey && ref === autoPendingKey) {
        // auto mode (#29): テンプレを受けたら Start 押下なしで即 -p 起動する。
        autoPendingKey = '';
        if (termRunning || pRunning) break; // 競合したら次の tick で再試行
        const extraArgs = beginRun();
        reviewRun = true;
        reviewedPrs[ref] = Date.now();
        chrome.storage.local.set({ reviewedPrs });
        port.postMessage({
          cmd: 'start',
          prompt: p,
          chrome: chromeEl.checked,
          extra_args: extraArgs,
          allowed_tools: reviewAllowedTools,
        });
        setStatus(`自動レビュー開始: ${ref}`);
        break;
      }
      if (termRunning) {
        // bracketed paste (ESC [200~ … ESC [201~) で包むと複数行でも submit されず
        // 1 ブロックで入る (claude は ?2004h を有効化済み)。送信の Enter は user が押す。
        port.postMessage({ cmd: 'term_input', data: `\x1b[200~${p}\x1b[201~` });
        if (term) term.focus();
        setStatus(`${ref} のレビュープロンプトを terminal に入力しました — Enter で送信`);
      } else {
        promptEl.value = p;
        setStatus(
          `${ref} のレビューテンプレを prompt に入れました — Start で開始 ` +
            `(read-only allowlist ${reviewAllowedTools.length} 件を自動適用)`
        );
      }
      break;
    }
    case 'raw':
      detectAuthError(msg.data);
      add('ev-stderr', `raw: ${msg.data}`);
      break;
    case 'stderr':
      detectAuthError(msg.data);
      add('ev-stderr', msg.data);
      break;
    case 'proc':
      if (msg.event === 'spawn') pRunning = true;
      if (msg.event === 'exit' || msg.event === 'killed') {
        pRunning = false;
        // auto mode: セッション終了後に一覧を再取得して次の未レビュー PR を拾う。
        if (autoEl.checked) setTimeout(loadDraftPrs, 3000);
      }
      if (msg.event === 'exit') {
        setStatus(`claude 終了 (code=${msg.code}) session_id=${msg.session_id || '?'}`);
      }
      if (msg.event === 'term_spawn') termRunning = true;
      if (msg.event === 'term_killed') termRunning = false;
      add('ev-proc', `proc: ${msg.event}${msg.code != null ? ` code=${msg.code}` : ''}`);
      break;
    case 'update':
      // host 起動時のバックグラウンド更新 (#6)。適用された時だけ届く。
      if (msg.component === 'extension') {
        addExtUpdated(msg.tag);
      } else {
        add('ev-proc', `agent を ${msg.tag} に更新しました (次回の起動から反映されます)`);
      }
      break;
    case 'update_status': {
      // 「更新確認」ボタンの結果 (最新でもフィードバックを出す)。
      const who = msg.component === 'extension' ? '拡張' : 'agent';
      if (msg.status === 'applied' && msg.component === 'extension') {
        addExtUpdated(msg.tag);
        setStatus('更新確認完了');
        break;
      }
      const text =
        msg.status === 'applied'
          ? `agent を ${msg.tag} に更新しました (次回の起動から反映されます)`
          : msg.status === 'up_to_date'
            ? `${who} は最新です`
            : msg.status === 'dev_build'
              ? 'ローカルビルドのため自動更新の対象外です'
              : `${who} の更新確認に失敗: ${msg.error || '?'}`;
      add(msg.status === 'error' ? 'ev-error' : 'ev-proc', text);
      setStatus('更新確認完了');
      break;
    }
    case 'debug_dump': {
      // 「debug コピー」の応答。クリップバードへ + 手動選択用に折りたたみでも出す
      // (clipboard が拒否された場合のフォールバック)。
      const lines = msg.lines || [];
      const text = lines.join('\n');
      addCollapsed(`debug dump (${lines.length} 件)`, text.slice(0, 8000));
      navigator.clipboard
        .writeText(text)
        .then(() => setStatus(`debug ログ ${lines.length} 件をコピーしました — そのまま貼り付けてください`))
        .catch((e) => setStatus(`クリップボード書き込み失敗 (${e}) — 下の折りたたみから手動でコピーしてください`));
      break;
    }
    case 'term_out':
      // replay で panel 再オープン時にも描き直せるよう、受信側でも ensure する。
      ensureTerm().write(b64ToBytes(msg.data || ''));
      break;
    case 'term_exit':
      termRunning = false;
      setStatus(`terminal 終了 (code=${msg.code})`);
      add('ev-proc', `terminal 終了 (code=${msg.code})`);
      termKillBtn.hidden = true;
      // 終了した terminal は畳む (死んだ画面を残さない)。出力の遡りは
      // debug コピー (term_exit note に先頭 2KB) で可能。
      termContainer.hidden = true;
      break;
    case 'busy':
      // auto の review_prompt → start が競合で弾かれた場合も含め、待ち状態を解除
      // して次の tick で再試行できるようにする (固着防止、Web Review 3 周目指摘)。
      autoPendingKey = '';
      setStatus('busy: 既にセッションが走っています (-p / terminal は同時 1 本)');
      break;
    case 'error':
      // review_prompt がエラーで返らなかった場合に auto が永久待ちにならないよう解除。
      autoPendingKey = '';
      add('ev-error', `error: ${msg.error}`);
      if (/unknown cmd: (review_prompt|resume)/.test(msg.error || '')) {
        add('ev-error', 'agent が古い可能性があります — 「更新確認」で agent を更新してください');
      }
      setStatus('error');
      break;
    case 'host_disconnected':
      // host 死亡 = claude も死んでいる (port 切断で kill)。実行中フラグと auto の
      // 待ち状態を戻して auto mode が固まらないようにする。
      pRunning = false;
      termRunning = false;
      autoPendingKey = '';
      setStatus(`host 切断${msg.error ? `: ${msg.error}` : ''}`);
      add('ev-error', `host 切断${msg.error ? `: ${msg.error}` : ''}`);
      break;
    default:
      addCollapsed(`msg: ${msg.type}`, JSON.stringify(msg, null, 2).slice(0, 4000));
  }
}

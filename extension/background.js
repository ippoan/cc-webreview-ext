// cc-webreview service worker (cc-webreview-ext#3, #4)
// - connectNative で native host (cc-webreview-agent) と繋ぐ
// - host からの message を console + ring buffer + side panel Port へ流す
// - {type:"chunk"} は再結合してから流す (host 側 1MB 対策の分割プロトコル)
// - side panel 再オープン時は buffer を replay して進行中セッションに再アタッチできるようにする

const HOST_NAME = 'com.ippoan.cc_webreview';
const BUFFER_MAX = 500;

let nativePort = null;
const panelPorts = new Set();
const eventBuffer = [];
const chunkBuf = new Map(); // id -> data 断片の配列

chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch(() => {});

function pushEvent(msg) {
  console.log('[cc-webreview]', msg);
  eventBuffer.push(msg);
  if (eventBuffer.length > BUFFER_MAX) eventBuffer.splice(0, eventBuffer.length - BUFFER_MAX);
  for (const p of panelPorts) {
    try {
      p.postMessage(msg);
    } catch (_) {
      panelPorts.delete(p);
    }
  }
}

// {type:"chunk", id, seq, last, data} を再結合する。完成したら parse して返す。
function reassemble(msg) {
  const parts = chunkBuf.get(msg.id) || [];
  parts[msg.seq] = msg.data;
  chunkBuf.set(msg.id, parts);
  if (!msg.last) return null;
  chunkBuf.delete(msg.id);
  try {
    return JSON.parse(parts.join(''));
  } catch (e) {
    return { type: 'error', error: `chunk 再結合失敗: ${e}` };
  }
}

function ensureNative() {
  if (nativePort) return nativePort;
  nativePort = chrome.runtime.connectNative(HOST_NAME);
  nativePort.onMessage.addListener((msg) => {
    if (msg && msg.type === 'chunk') {
      const full = reassemble(msg);
      if (full) pushEvent(full);
      return;
    }
    pushEvent(msg);
  });
  nativePort.onDisconnect.addListener(() => {
    const err = chrome.runtime.lastError ? chrome.runtime.lastError.message : null;
    nativePort = null;
    chunkBuf.clear();
    pushEvent({ type: 'host_disconnected', error: err });
  });
  return nativePort;
}

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== 'panel') return;
  panelPorts.add(port);
  port.onDisconnect.addListener(() => panelPorts.delete(port));
  port.onMessage.addListener((msg) => {
    if (!msg || !msg.cmd) return;
    if (msg.cmd === 'replay') {
      for (const e of eventBuffer) port.postMessage(e);
      return;
    }
    // panel の「log クリア」。replay バッファを空にする (host へは送らない)。
    if (msg.cmd === 'clear_log') {
      eventBuffer.length = 0;
      return;
    }
    // start / stop / ping は host へそのまま中継。
    try {
      ensureNative().postMessage(msg);
    } catch (e) {
      pushEvent({ type: 'error', error: `native host へ送信失敗: ${e}` });
    }
  });
});

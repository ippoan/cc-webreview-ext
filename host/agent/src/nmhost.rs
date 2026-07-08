// SOURCE-MIRROR: ippoan/cdp-relay:rust/agent/src/nmhost.rs (framing 部分)
//! Native Messaging framing + 1MB 上限対策のチャンク分割 (cc-webreview-ext#3)。
//!
//! Chrome の Native Messaging は 4-byte LE length prefix + UTF-8 JSON。
//! host → Chrome は **1 message 1MB 上限**なので、閾値を超える message は
//! `{type:"chunk", id, seq, last, data}` に分割し、拡張 (background.js) 側で再結合する。
//!
//! stdout は native messaging チャネルなので framed JSON 以外を絶対に出さない
//! (log は stderr)。framing / 分割は OS 非依存の純関数にして CCoW (Linux) で unit test する。

use serde_json::{json, Value};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 拡張 → host の 1 メッセージ上限 (制御メッセージ用途なので小さく cap)。
pub const MAX_INBOUND_BYTES: usize = 1024 * 1024;

/// host → Chrome の分割閾値。Chrome の上限 1MB に対して余裕を持たせる。
pub const CHUNK_THRESHOLD: usize = 512 * 1024;

/// 1 チャンクに載せる data (シリアライズ済み JSON 文字列の断片) の最大バイト数。
/// chunk メッセージ自体の envelope 分を含めても閾値内に収まるサイズ。
pub const CHUNK_DATA_BYTES: usize = 256 * 1024;

/// stdin から 4-byte LE length-prefixed JSON を 1 件読む。EOF (Chrome が port を閉じた) は
/// `Ok(None)`。length 超過や不正 JSON は `Err`。
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_INBOUND_BYTES {
        return Err(io::Error::new(ErrorKind::InvalidData, "message too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let v = serde_json::from_slice(&buf)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(v))
}

/// writer に 4-byte LE length-prefixed JSON を 1 件書く。
pub fn write_message<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(v).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "message too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// message を transport に載せる形に変換する純関数。シリアライズ後のサイズが
/// `threshold` 以下ならそのまま 1 件、超えるなら `{type:"chunk"}` の列に分割する。
/// data は必ず UTF-8 の char 境界で切る (serde_json の出力は valid UTF-8)。
pub fn split_for_transport(v: &Value, threshold: usize, chunk_bytes: usize, id: u64) -> Vec<Value> {
    let serialized = v.to_string();
    if serialized.len() <= threshold {
        return vec![v.clone()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in serialized.chars() {
        if cur.len() + ch.len_utf8() > chunk_bytes {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    let last_idx = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(seq, data)| {
            json!({
                "type": "chunk",
                "id": id,
                "seq": seq,
                "last": seq == last_idx,
                "data": data,
            })
        })
        .collect()
}

/// スレッド間で共有する framed writer。claude stdout/stderr の読み取りスレッドと
/// メインループが同じ stdout に書くため、message 単位で排他する。
pub struct SharedWriter<W: Write> {
    inner: Arc<Mutex<W>>,
    chunk_id: AtomicU64,
}

impl<W: Write> SharedWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(w)),
            chunk_id: AtomicU64::new(0),
        }
    }

    /// message を (必要なら分割して) 送る。lock poisoning は host 終了で良いので unwrap。
    pub fn send(&self, v: &Value) -> io::Result<()> {
        let id = self.chunk_id.fetch_add(1, Ordering::Relaxed);
        let parts = split_for_transport(v, CHUNK_THRESHOLD, CHUNK_DATA_BYTES, id);
        let mut w = self.inner.lock().unwrap();
        for p in &parts {
            write_message(&mut *w, p)?;
        }
        Ok(())
    }
}

// SharedWriter は Clone しない。chunk id の空間を共有するため、スレッドへ渡す時は
// Arc<SharedWriter<W>> で包んで共有すること (clone すると id が衝突し再結合が壊れる)。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips() {
        let msg = json!({ "cmd": "start", "prompt": "レビューして" });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);

        let mut cursor = io::Cursor::new(buf);
        let got = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(got, msg);
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_message_rejects_oversized_length() {
        let len = (MAX_INBOUND_BYTES as u32) + 1;
        let mut buf = len.to_le_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let mut cursor = io::Cursor::new(buf);
        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn small_message_is_not_chunked() {
        let v = json!({ "type": "claude", "data": { "x": 1 } });
        let parts = split_for_transport(&v, CHUNK_THRESHOLD, CHUNK_DATA_BYTES, 0);
        assert_eq!(parts, vec![v]);
    }

    #[test]
    fn large_message_chunks_and_reassembles() {
        // 日本語 (multibyte) 込みの大きい payload で char 境界の安全性ごと確認する。
        let payload = "あいうえおabc".repeat(2000);
        let v = json!({ "type": "claude", "data": payload });
        let parts = split_for_transport(&v, 1024, 300, 7);
        assert!(parts.len() > 1);

        let mut joined = String::new();
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p["type"], "chunk");
            assert_eq!(p["id"], 7);
            assert_eq!(p["seq"], i);
            assert_eq!(p["last"], i == parts.len() - 1);
            // 各チャンクの data は指定サイズ以下。
            assert!(p["data"].as_str().unwrap().len() <= 300);
            joined.push_str(p["data"].as_str().unwrap());
        }
        let restored: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(restored, v);
    }

    #[test]
    fn shared_writer_sends_framed() {
        let w = SharedWriter::new(Vec::new());
        w.send(&json!({ "type": "hello" })).unwrap();
        let buf = w.inner.lock().unwrap().clone();
        let mut cursor = io::Cursor::new(buf);
        let got = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(got["type"], "hello");
    }
}

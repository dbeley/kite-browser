//! Chrome DevTools Protocol client: flat-session aware request/response plus an
//! event queue, all blocking, all on one connection.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::ws::{self, WsWriter};

#[derive(Debug, Clone)]
pub struct Event {
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

pub struct Cdp {
    writer: Arc<Mutex<WsWriter>>,
    rx: Receiver<String>,
    next_id: u64,
    queue: VecDeque<Event>,
}

impl Cdp {
    pub fn connect(ws_url: &str) -> Result<Cdp, String> {
        let (mut reader, writer) = ws::connect(ws_url).map_err(|e| format!("ws connect: {e}"))?;
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || loop {
                match reader.next_text() {
                    Ok(Some(t)) => {
                        if tx.send(t).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => return,
                }
            })
            .map_err(|e| format!("spawn reader: {e}"))?;
        Ok(Cdp {
            writer,
            rx,
            next_id: 1,
            queue: VecDeque::new(),
        })
    }

    fn send(&self, payload: &str) -> Result<(), String> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| "ws lock poisoned".to_string())?;
        w.send_text(payload).map_err(|e| format!("ws send: {e}"))
    }

    /// Blocking call. Events arriving while we wait are queued, not lost.
    pub fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.send(&msg.to_string())?;

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| Duration::from_millis(1));
            let text = match self.rx.recv_timeout(left) {
                Ok(t) => t,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("timeout waiting for {method}"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("engine connection closed".to_string());
                }
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    if !err.is_null() {
                        let msg = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("protocol error");
                        return Err(format!("{method}: {msg}"));
                    }
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                self.queue.push_back(Event {
                    session: v
                        .get("sessionId")
                        .and_then(|s| s.as_str())
                        .map(str::to_string),
                    method: m.to_string(),
                    params: v.get("params").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    /// Wait up to `timeout` for the next event.
    pub fn wait_event(&mut self, timeout: Duration) -> Option<Event> {
        if let Some(e) = self.queue.pop_front() {
            return Some(e);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(text) => {
                let v: Value = serde_json::from_str(&text).ok()?;
                let m = v.get("method")?.as_str()?.to_string();
                Some(Event {
                    session: v
                        .get("sessionId")
                        .and_then(|s| s.as_str())
                        .map(str::to_string),
                    method: m,
                    params: v.get("params").cloned().unwrap_or(Value::Null),
                })
            }
            Err(_) => None,
        }
    }

    /// Convenience: evaluate JS in a session, returning the JSON value.
    pub fn eval(&mut self, session: Option<&str>, expr: &str) -> Result<Value, String> {
        self.eval_ctx(session, None, expr)
    }

    /// Evaluate in a specific execution context (the shell's isolated world).
    pub fn eval_ctx(
        &mut self,
        session: Option<&str>,
        ctx: Option<i64>,
        expr: &str,
    ) -> Result<Value, String> {
        let mut params = json!({
            "expression": expr,
            "returnByValue": true,
            "awaitPromise": false,
            "userGesture": true,
        });
        if let Some(c) = ctx {
            params["contextId"] = json!(c);
        }
        let r = self.call(session, "Runtime.evaluate", params)?;
        if let Some(ex) = r.get("exceptionDetails") {
            if !ex.is_null() {
                let msg = ex
                    .pointer("/exception/description")
                    .and_then(|d| d.as_str())
                    .or_else(|| ex.get("text").and_then(|t| t.as_str()))
                    .unwrap_or("javascript error");
                return Err(msg.to_string());
            }
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }
}

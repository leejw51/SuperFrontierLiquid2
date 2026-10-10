//! The websocket, dialled the way the browser dials it.
//!
//! # The path is derived, not typed in
//!
//! Fault 4 was that the gateway serves `/ws` at the server root and the
//! frontend asked for `/api/v1/ws`. Both sides were individually correct and
//! individually tested; the socket simply never connected, and a browser
//! reports that only as a failed handshake with no body.
//!
//! A test that connected to a hardcoded `"/ws"` would have passed throughout
//! that outage, because the gateway was serving `/ws` the whole time. The only
//! test that catches it is one that asks the *frontend's* question. So
//! [`frontend_websocket_url`] is a port of `cb-web`'s `websocket_url`, and
//! `tests/regressions.rs` guards the port against the shipped frontend source
//! drifting away from it.
//!
//! `cb-web` cannot simply be called: it is excluded from the workspace and its
//! relative-URL branch reads `window.location`, so it only compiles for
//! `wasm32`. The port covers the absolute-URL branch, which is the one a test
//! can exercise and the one the deployed configuration uses.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Turns an HTTP API base into the websocket URL, exactly as `cb-web` does.
///
/// Ported from `rust/crates/cb-web/src/config.rs`. The API path is stripped
/// before the socket path is attached, so `http://host:8081/api/v1` becomes
/// `ws://host:8081/ws` rather than `ws://host:8081/api/v1/ws`.
pub fn frontend_websocket_url(api_base: &str) -> String {
    fn authority(rest: &str) -> &str {
        rest.split('/').next().unwrap_or(rest)
    }
    if let Some(rest) = api_base.strip_prefix("https://") {
        return format!("wss://{}/ws", authority(rest));
    }
    if let Some(rest) = api_base.strip_prefix("http://") {
        return format!("ws://{}/ws", authority(rest));
    }
    format!("ws://{}/ws", authority(api_base))
}

/// The naive derivation — appending the socket path to the API base — which is
/// what the frontend used to do and what nothing must serve.
///
/// Kept so the path test can show it discriminates. A test that only proves the
/// right URL works has not proved the wrong one fails.
pub fn naive_websocket_url(api_base: &str) -> String {
    let base = api_base
        .strip_prefix("http://")
        .map(|rest| format!("ws://{rest}"))
        .or_else(|| {
            api_base
                .strip_prefix("https://")
                .map(|rest| format!("wss://{rest}"))
        })
        .unwrap_or_else(|| api_base.to_string());
    format!("{base}/ws")
}

/// A connected socket that remembers everything it has been sent.
///
/// Ordering claims — "a snapshot arrives before any delta" — are assertions
/// about the sequence, and absence claims — "no fill of another user's ever
/// arrives" — are assertions about the whole sequence. Both need the history,
/// so every frame read is kept.
pub struct Ws {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    received: Vec<Value>,
}

impl Ws {
    /// Dials a websocket URL, failing with the URL and the server's answer.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (stream, response) = tokio_tungstenite::connect_async(url).await?;
        assert_eq!(
            response.status().as_u16(),
            101,
            "{url} did not upgrade; the server answered {}",
            response.status()
        );
        Ok(Self {
            stream,
            received: Vec::new(),
        })
    }

    /// Sends one client frame.
    pub async fn send(&mut self, value: Value) {
        self.stream
            .send(Message::Text(value.to_string()))
            .await
            .unwrap_or_else(|err| panic!("cannot send {value} on the websocket: {err}"));
    }

    /// Subscribes to channels.
    pub async fn subscribe(&mut self, channels: &[&str]) {
        self.send(serde_json::json!({ "op": "subscribe", "channels": channels }))
            .await;
    }

    /// Authenticates the socket with a session token.
    pub async fn auth(&mut self, token: &str) {
        self.send(serde_json::json!({ "op": "auth", "token": token }))
            .await;
    }

    /// Everything received so far, in arrival order.
    pub fn history(&self) -> &[Value] {
        &self.received
    }

    /// Reads one frame, or `None` if none arrives within `timeout`.
    pub async fn next_within(&mut self, timeout: Duration) -> Option<Value> {
        loop {
            match tokio::time::timeout(timeout, self.stream.next()).await {
                Err(_) => return None,
                Ok(None) => return None,
                Ok(Some(Err(err))) => panic!("websocket error: {err}"),
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: Value = serde_json::from_str(&text)
                        .unwrap_or_else(|err| panic!("websocket sent non-JSON {text:?}: {err}"));
                    self.received.push(value.clone());
                    return Some(value);
                }
                // Ping, pong, binary and close frames are transport, not
                // protocol; keep reading for the next protocol message.
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("websocket closed by the server: {frame:?}")
                }
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    /// Reads until a frame matches, keeping everything read on the way.
    ///
    /// Fails with the whole history, because "the delta never arrived" is
    /// unactionable and "here are the nine frames that did arrive" is not.
    pub async fn await_message<F>(&mut self, what: &str, timeout: Duration, mut matches: F) -> Value
    where
        F: FnMut(&Value) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "timed out after {:.1}s waiting for {what}. Frames received:\n{}",
                    timeout.as_secs_f64(),
                    self.describe_history()
                );
            }
            match self.next_within(remaining).await {
                Some(value) if matches(&value) => return value,
                Some(_) => continue,
                None => panic!(
                    "the socket went quiet while waiting for {what}. Frames received:\n{}",
                    self.describe_history()
                ),
            }
        }
    }

    /// Drains whatever has already arrived, without waiting for more.
    ///
    /// Used to settle a socket before an absence assertion: the claim is "no
    /// such frame arrived by now", and "now" has to be pinned by draining.
    pub async fn drain(&mut self, quiet_for: Duration) {
        while self.next_within(quiet_for).await.is_some() {}
    }

    /// The history, one `channel/type` per line.
    pub fn describe_history(&self) -> String {
        if self.received.is_empty() {
            return "  (none)".to_string();
        }
        self.received
            .iter()
            .map(|value| {
                format!(
                    "  {} / {}",
                    value["channel"].as_str().unwrap_or("?"),
                    value["type"].as_str().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every frame on a channel with a given type.
    pub fn frames(&self, channel: &str, kind: &str) -> Vec<&Value> {
        self.received
            .iter()
            .filter(|value| {
                value["channel"].as_str() == Some(channel) && value["type"].as_str() == Some(kind)
            })
            .collect()
    }
}

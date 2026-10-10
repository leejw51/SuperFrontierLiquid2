//! The real websocket transport.
//!
//! One connection attempt per [`Transport::connect`] call, with no retry logic
//! of its own: retrying is the driver's job, and splitting it across two places
//! is how a reconnection loop ends up with two different opinions about how long
//! to wait.
//!
//! Binary frames are ignored rather than decoded. The contract is JSON text; a
//! binary frame from a gateway speaking this contract would be a bug on the
//! other side, and guessing at it here would hide that.

use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;
use wasm_bindgen_futures::spawn_local;

use super::{Socket, Transport};

/// A transport that opens a real websocket against the gateway.
pub struct WebSocketTransport {
    url: String,
}

impl WebSocketTransport {
    /// Creates a transport for a `ws://` or `wss://` endpoint.
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
        }
    }
}

impl Transport for WebSocketTransport {
    fn connect(&self) -> LocalBoxFuture<'static, Result<Socket, String>> {
        let url = self.url.clone();
        async move {
            let socket = WebSocket::open(&url).map_err(|error| error.to_string())?;
            let (mut write, read) = socket.split();
            let (outgoing, mut to_send) = mpsc::unbounded::<String>();

            // Pump outgoing frames until either side hangs up. Ending this task
            // on the first send error is deliberate: the read half is about to
            // end too, and the driver reconnects from there.
            spawn_local(async move {
                while let Some(text) = to_send.next().await {
                    if write.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                let _ = write.close().await;
            });

            let incoming = read
                .take_while(|frame| futures::future::ready(frame.is_ok()))
                .filter_map(|frame| async move {
                    match frame {
                        Ok(Message::Text(text)) => Some(text),
                        _ => None,
                    }
                })
                .boxed_local();

            Ok(Socket { incoming, outgoing })
        }
        .boxed_local()
    }
}

//! `WS /api/ws/control` — per-client control channel.
//!
//! Wire protocol (JSON, `{"type": "..."}` tagged):
//!
//! Client → Server:
//!   - `{"type": "hello", "label": "Hammer on ThinkPad"}` — identifies the client
//!   - `{"type": "ptt_start"}` — request to key
//!   - `{"type": "ptt_heartbeat"}` — keep-alive (send every 150 ms while keyed)
//!   - `{"type": "ptt_stop"}` — release
//!
//! Server → Client:
//!   - `{"type": "welcome", "client_id": "c42"}`
//!   - `{"type": "ptt_granted"}`
//!   - `{"type": "ptt_denied", "reason": "...", "owner": "..."}`
//!   - `{"type": "ptt_acquired", "owner": "..."}` (any client becomes owner)
//!   - `{"type": "ptt_released", "previous": "...", "reason": "..."}`
//!   - `{"type": "error", "message": "..."}`

use crate::ptt::{ClientId, Grant, PttEvent, PttHandle, ReleaseReason};
use axum::extract::ws::{Message, WebSocket};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

static CLIENT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_client_id() -> ClientId {
    let n = CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{n}")
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Hello { label: Option<String> },
    PttStart,
    PttHeartbeat,
    PttStop,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Welcome {
        client_id: &'a str,
    },
    PttGranted,
    PttDenied {
        reason: &'a str,
        owner: Option<&'a str>,
    },
    PttAcquired {
        owner: &'a str,
    },
    PttReleased {
        previous: &'a str,
        reason: &'a str,
    },
    Error {
        message: &'a str,
    },
}

async fn send<'a>(sock: &mut WebSocket, msg: ServerMsg<'a>) -> bool {
    match serde_json::to_string(&msg) {
        Ok(s) => sock.send(Message::Text(s)).await.is_ok(),
        Err(e) => {
            warn!(error = ?e, "ws json encode");
            false
        }
    }
}

pub async fn handle(mut socket: WebSocket, ptt: PttHandle) {
    let client_id = next_client_id();
    let mut label: String = format!("anon-{client_id}");
    info!(client_id = %client_id, "control ws connected");

    // First message: welcome.
    if !send(
        &mut socket,
        ServerMsg::Welcome {
            client_id: &client_id,
        },
    )
    .await
    {
        return;
    }

    let mut events = ptt.subscribe();
    // If someone already has PTT when we connect, tell them so the UI starts correct.
    if let Some(owner) = ptt.query_owner().await {
        let _ = send(
            &mut socket,
            ServerMsg::PttAcquired {
                owner: &owner.label,
            },
        )
        .await;
    }

    loop {
        tokio::select! {
            biased;
            ev = events.recv() => match ev {
                Ok(PttEvent::Acquired(o)) => {
                    if !send(&mut socket, ServerMsg::PttAcquired { owner: &o.label }).await { break; }
                }
                Ok(PttEvent::Released { previous, reason }) => {
                    if !send(
                        &mut socket,
                        ServerMsg::PttReleased {
                            previous: &previous.label,
                            reason: reason.as_str(),
                        },
                    ).await { break; }
                }
                Err(RecvError::Lagged(_)) => {
                    // Pubsub overrun — not critical, just note it.
                    let _ = send(&mut socket, ServerMsg::Error { message: "event lag" }).await;
                }
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                None => break,
                Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(ClientMsg::Hello { label: Some(l) }) if !l.is_empty() => {
                            label = l;
                            info!(client_id = %client_id, label = %label, "hello");
                        }
                        Ok(ClientMsg::Hello { .. }) => {}
                        Ok(ClientMsg::PttStart) => {
                            let grant = ptt.start(client_id.clone(), label.clone()).await;
                            let ok = match &grant {
                                Grant::Granted => send(&mut socket, ServerMsg::PttGranted).await,
                                Grant::DeniedLockedBy(who) => {
                                    send(&mut socket, ServerMsg::PttDenied {
                                        reason: "locked_by",
                                        owner: Some(who),
                                    }).await
                                }
                                Grant::DeniedRadioError(e) => {
                                    send(&mut socket, ServerMsg::PttDenied {
                                        reason: "radio_error",
                                        owner: None,
                                    }).await &&
                                    send(&mut socket, ServerMsg::Error { message: e }).await
                                }
                                Grant::DeniedArbiterDown => {
                                    send(&mut socket, ServerMsg::PttDenied {
                                        reason: "arbiter_down",
                                        owner: None,
                                    }).await
                                }
                            };
                            if !ok { break; }
                        }
                        Ok(ClientMsg::PttHeartbeat) => {
                            ptt.heartbeat(client_id.clone()).await;
                        }
                        Ok(ClientMsg::PttStop) => {
                            ptt.stop(client_id.clone()).await;
                        }
                        Err(e) => {
                            let msg = format!("bad json: {e}");
                            let _ = send(&mut socket, ServerMsg::Error { message: &msg }).await;
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = send(&mut socket, ServerMsg::Error {
                        message: "binary frames not supported on control ws",
                    }).await;
                }
                Some(Ok(Message::Ping(p))) => { let _ = socket.send(Message::Pong(p)).await; }
                Some(Ok(Message::Pong(_))) => {}
            },
        }
    }

    // On disconnect: signal to the arbiter. If this client held PTT, this
    // causes an immediate `E` and a ReleaseReason::SocketClose.
    ptt.client_gone(client_id.clone()).await;
    info!(client_id = %client_id, "control ws disconnected");
    // Suppress unused-warning on ReleaseReason (the static matrix still lives
    // in the module; compiler notices unused variants in debug if we ever
    // remove a branch above).
    let _ = ReleaseReason::UserStop;
}

use bastion::context::BastionContext;
use bastion::distributor::Distributor;
use bastion::message::MessageHandler;
use bastion::supervisor::{RestartPolicy, RestartStrategy, SupervisorRef};
use bastion::{blocking, Bastion};
use futures::Stream;
use gst_sdp::SDPMessage;

use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::sink::{Sink, SinkExt};
use futures::stream::StreamExt;

use async_tungstenite::tungstenite::Error as WsError;
use async_tungstenite::tungstenite::Message as WsMessage;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::webrtc_actor::WebRtcActor;
use crate::webrtcbin_actor::SDPType;

const WS_SERVER: &str = "wss://192.168.1.21:8443";

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u16,
        #[serde(rename = "sdpMid")]
        sdp_mid: String,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

pub struct WsActor;

impl WsActor {
    pub fn run(parent: SupervisorRef, order: u8) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never),
                )
                .children(move |c| {
                    c.with_distributor(Distributor::named(format!("web_socket_{}", order)))
                        .with_exec(move |ctx| async_main(ctx, order))
                })
            })
            .expect("couldn't run WebRTC actor");
    }

    fn handle_websocket_message(msg: &str, order: u8) -> Result<(), anyhow::Error> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {}", msg);
        }

        let json_msg: JsonMsg = serde_json::from_str(&msg)?;
        println!("{json_msg:?}");

        match json_msg {
            JsonMsg::Sdp { type_, sdp } => {
                let msg = json!({
                    "type": type_,
                    "sdp": sdp
                });
                if &type_ == "offer" {
                    let server_parent = Bastion::supervisor(|s| s).unwrap();
                    // WebRtcActor::run(server_parent, &msg.to_string(), order);
                }
            }
            JsonMsg::Ice {
                candidate,
                sdp_mline_index,
                sdp_mid,
            } => {
                let msg = json!({
                    "candidate": candidate,
                    "sdp_mid": sdp_mid,
                    "sdp_mline_index": sdp_mline_index,
                    "username_fragment": String::new()
                });
                Distributor::named(format!("webrtc_{order}"))
                    .tell_one((candidate, sdp_mline_index, sdp_mid))
                    .expect("couldn't send ICE to WebRTC actor");
            }
        };

        Ok(())
    }
}

async fn async_main(ctx: BastionContext, order: u8) -> Result<(), ()> {
    let (mut ws, _) =
        async_tungstenite::async_std::connect_async_with_tls_connector(WS_SERVER, None)
            .await
            .map_err(|e| eprintln!("{}", e))?;

    let our_id = order;
    ws.send(WsMessage::Text(format!("HELLO {}", our_id)))
        .await
        .map_err(|e| eprintln!("{}", e))?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))
        .map_err(|e| eprintln!("{}", e))?
        .map_err(|e| eprintln!("error"))?;

    if msg != WsMessage::Text("HELLO".into()) {
        eprintln!("server {} didn't say HELLO", order);
    }

    let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded::<WsMessage>();

    blocking!(run(send_ws_msg_rx, ws, order).await);

    println!("WsActor_{order} started!");
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|(sdp_type, sdp): (SDPType, String), _| {
                let msg = serde_json::to_string(&JsonMsg::Sdp {
                    type_: sdp_type.to_str().to_owned(),
                    sdp,
                })
                .unwrap();
                println!("SEND:\n{msg}");
                send_ws_msg_tx.unbounded_send(WsMessage::Text(msg));
            })
            .on_tell(
                |(sdp_mline_index, candidate, sdp_mid): (u16, String, String), _| {
                    let msg = serde_json::to_string(&JsonMsg::Ice {
                        candidate,
                        sdp_mline_index,
                        sdp_mid,
                    })
                    .unwrap();
                    println!("SEND:\t{msg}");
                    send_ws_msg_tx.unbounded_send(WsMessage::Text(msg));
                },
            );
    }
}

async fn run(
    send_ws_msg_rx: UnboundedReceiver<WsMessage>,
    ws: impl Sink<WsMessage, Error = WsError> + Stream<Item = Result<WsMessage, WsError>>,
    order: u8,
) -> Result<(), ()> {
    let (mut ws_sink, ws_stream) = ws.split();

    let mut ws_stream = ws_stream.fuse();
    let mut send_ws_msg_rx = send_ws_msg_rx.fuse();

    loop {
        let ws_msg = futures::select! {
            ws_msg = ws_stream.select_next_some() => {
                match ws_msg.map_err(|e| eprintln!("{}", e))? {
                    WsMessage::Close(_) => {
                        println!("peer disconnected");
                        break
                    },
                    WsMessage::Ping(data) => Some(WsMessage::Pong(data)),
                    WsMessage::Pong(_) => None,
                    WsMessage::Binary(_) => None,
                    WsMessage::Text(text) => {
                        WsActor::handle_websocket_message(&text, order).map_err(|e| eprintln!("{}", e))?;
                        None
                    },
                }
            }
            ws_msg = send_ws_msg_rx.select_next_some() => Some(ws_msg),
            complete => break,
        };

        if let Some(ws_msg) = ws_msg {
            ws_sink.send(ws_msg).await.map_err(|e| eprintln!("{}", e))?;
        }
    }

    Ok(())
}

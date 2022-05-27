use bastion::blocking;
use bastion::context::BastionContext;
use bastion::distributor::Distributor;
use bastion::message::MessageHandler;
use bastion::supervisor::{RestartPolicy, RestartStrategy, SupervisorRef};
use futures::Stream;
use gst_sdp::SDPMessage;

use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::sink::{Sink, SinkExt};
use futures::stream::StreamExt;

use async_tungstenite::tungstenite::Error as WsError;
use async_tungstenite::tungstenite::Message as WsMessage;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};

use crate::webrtcbin_actor::SDPType;

const WS_SERVER: &str = "wss://webrtc.nirbheek.in:8443";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

pub struct WsActor;

impl WsActor {
    pub fn run(parent: SupervisorRef) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never), // .with_restart_policy(RestartPolicy::Tries(5))
                                                                                          // .with_actor_restart_strategy(ActorRestartStrategy::Immediate),
                )
                .children(|c| {
                    c.with_distributor(Distributor::named("web_socket"))
                        .with_exec(async_main)
                })
            })
            .expect("couldn't run WebRTC actor");
    }

    fn handle_websocket_message(msg: &str) -> Result<(), anyhow::Error> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {}", msg);
        }

        let json_msg: JsonMsg = serde_json::from_str(msg)?;

        let webrtcbin = Distributor::named("server");

        match json_msg {
            JsonMsg::Sdp { type_, sdp } => {
                let type_ = match type_.as_str() {
                    "offer" => SDPType::Offer,
                    "answer" => SDPType::Answer,
                    _ => bail!("sdp type not supported"),
                };
                let sdp = SDPMessage::parse_buffer(sdp.as_bytes())?;
                webrtcbin.tell_one((type_, sdp))
            }
            JsonMsg::Ice {
                sdp_mline_index,
                candidate,
            } => webrtcbin.tell_one((sdp_mline_index, candidate)),
        };

        Ok(())
    }
}

async fn async_main(ctx: BastionContext) -> Result<(), ()> {
    let (mut ws, _) = async_tungstenite::async_std::connect_async(WS_SERVER)
        .await
        .map_err(|e| eprintln!("{}", e))?;

    let our_id = 1212;
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
        eprintln!("server didn't say HELLO");
    }

    let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded::<WsMessage>();

    blocking!(run(send_ws_msg_rx, ws).await);

    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|(sdp_type, sdp): (SDPType, SDPMessage), _| {
                println!("prepare to send sdp");
                let msg = serde_json::to_string(&JsonMsg::Sdp {
                    type_: sdp_type.to_str().to_owned(),
                    sdp: sdp.as_text().unwrap(),
                })
                .unwrap();
                send_ws_msg_tx.unbounded_send(WsMessage::Text(msg));
            })
            .on_tell(|(mlineindex, candidate): (u32, String), _| {
                println!("prepare to send ice candidate");
                let msg = serde_json::to_string(&JsonMsg::Ice {
                    candidate,
                    sdp_mline_index: mlineindex,
                })
                .unwrap();
                send_ws_msg_tx.unbounded_send(WsMessage::Text(msg));
            });
    }
}

async fn run(
    send_ws_msg_rx: UnboundedReceiver<WsMessage>,
    ws: impl Sink<WsMessage, Error = WsError> + Stream<Item = Result<WsMessage, WsError>>,
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
                        WsActor::handle_websocket_message(&text).map_err(|e| eprintln!("{}", e))?;
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

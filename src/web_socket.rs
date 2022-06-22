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
    pub fn run(parent: SupervisorRef, order: u8, room_id: u16) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never),
                )
                .children(move |c| {
                    c.with_distributor(Distributor::named(format!("web_socket_{}", order)))
                        .with_exec(move |ctx| async_main(ctx, order, room_id))
                })
            })
            .expect("couldn't run WebRTC actor");
    }

    fn handle_websocket_message(msg: &str, order: u8) -> Result<(), anyhow::Error> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {}", msg);
        }

        let webrtcbin = Distributor::named(format!("server_{}", order));

        if msg.starts_with("ROOM_PEER_MSG ") {
            let mut split = msg["ROOM_PEER_MSG ".len()..].splitn(2, ' ');
            let peer_id = split
                .next()
                .and_then(|s| str::parse::<u32>(s).ok())
                .ok_or_else(|| anyhow::anyhow!("can't parse peer id"))?;

            let msg = split
                .next()
                .ok_or_else(|| anyhow::anyhow!("can't parse peer message"))?;
git 
            let json_msg: JsonMsg = serde_json::from_str(msg)?;
            match json_msg {
                JsonMsg::Sdp { type_, sdp } => {
                    let type_ = match type_.as_str() {
                        "offer" => SDPType::Offer,
                        "answer" => SDPType::Answer,
                        _ => bail!("sdp type not supported"),
                    };
                    let sdp = SDPMessage::parse_buffer(sdp.as_bytes())?;
                    webrtcbin.tell_one((peer_id, (type_, sdp)))
                }
                JsonMsg::Ice {
                    sdp_mline_index,
                    candidate,
                } => webrtcbin.tell_one((peer_id, (sdp_mline_index, candidate))),
            };
        } else if msg.starts_with("ROOM_PEER_JOINED") {
            let mut split = msg["ROOM_PEER_JOINED ".len()..].splitn(2, ' ');
            let peer_id = split.next().ok_or_else(|| anyhow!("Can't parse peer id"))?;

            webrtcbin.tell_one(("add", peer_id.to_owned()));
        } else if msg.starts_with("ROOM_PEER_LEFT") {
            let mut split = msg["ROOM_PEER_LEFT ".len()..].splitn(2, ' ');
            let peer_id = split.next().ok_or_else(|| anyhow!("Can't parse peer id"))?;

            webrtcbin.tell_one(("remove", peer_id.to_owned()));
        }

        Ok(())
    }
}

async fn async_main(ctx: BastionContext, order: u8, room_id: u16) -> Result<(), ()> {
    let (mut ws, _) = async_tungstenite::async_std::connect_async(WS_SERVER)
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

    ws.send(WsMessage::Text(format!("ROOM {room_id}")))
        .await
        .unwrap();

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))
        .map_err(|e| eprintln!("{}", e))?
        .map_err(|e| eprintln!("error"))?;

    if let WsMessage::Text(text) = &msg {
        if !text.starts_with("ROOM_OK") {
            println!("server error: {:?}", text);
        }

        println!("Joined room {room_id}");
    } else {
        println!("server error: {:?}", msg);
    }

    let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded::<WsMessage>();

    blocking!(run(send_ws_msg_rx, ws, order).await);

    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|(sdp_type, sdp): (SDPType, SDPMessage), _| {
                println!("SEND:\n{}", sdp.as_text().unwrap());
                let msg = serde_json::to_string(&JsonMsg::Sdp {
                    type_: sdp_type.to_str().to_owned(),
                    sdp: sdp.as_text().unwrap(),
                })
                .unwrap();
                send_ws_msg_tx.unbounded_send(WsMessage::Text(msg));
            })
            .on_tell(|(mlineindex, candidate): (u32, String), _| {
                println!("SEND:\t{}", candidate);
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

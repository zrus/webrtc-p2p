use std::sync::Arc;

use bastion::{
    spawn,
    supervisor::{RestartPolicy, RestartStrategy, SupervisorRef},
};
use tokio::{net::UdpSocket, select};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_VP8, MIME_TYPE_H264},
        APIBuilder,
    },
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{
        track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter,
    },
    Error,
};

use crate::gstreamer_actor::GstreamerActor;

pub struct WebRtcActor;

impl WebRtcActor {
    pub fn run(parent: SupervisorRef, sdp: &str) {
        let sdp = sdp.to_owned();
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never), // .with_restart_policy(RestartPolicy::Tries(5))
                                                                                          // .with_actor_restart_strategy(ActorRestartStrategy::Immediate),
                )
                .children(|c| {
                    c.with_exec(move |ctx| {
                        println!("WebRTC started");
                        let sdp = sdp.clone();
                        GstreamerActor::run(ctx.supervisor().unwrap().supervisor(|s| s).unwrap());
                        main_fn(sdp)
                    })
                })
            })
            .expect("couldn't run WebRTC actor");
    }
}

async fn main_fn(sdp: String) -> Result<(), ()> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()
        .expect("couldn't register default codec");

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)
        .expect("couldn't register default interceptors");
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["turn:turn.tel4vn.com:5349?transport=tcp".to_owned()],
            username: "tel4vn".to_owned(),
            credential: "TEL4VN.COM".to_owned(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .expect("couldn't create new peer connection"),
    );

    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .expect("couldn't add track");

    spawn!(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<(), ()>::Ok(())
    });

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let done_tx1 = done_tx.clone();

    peer_connection
        .on_ice_connection_state_change(Box::new(move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {}", connection_state);
            if connection_state == RTCIceConnectionState::Failed {
                let _ = done_tx1.try_send(());
            }
            Box::pin(async {})
        }))
        .await;

    let done_tx2 = done_tx.clone();

    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("Peer Connection State has changed: {}", s);

            if s == RTCPeerConnectionState::Failed {
                println!("Peer Connection has gone to failed exiting: Done forwarding");
                let _ = done_tx2.try_send(());
            }

            Box::pin(async {})
        }))
        .await;

    let bdata = base64::decode(sdp).expect("couldn't decode SDP");
    let desc_data = String::from_utf8(bdata).expect("couldn't create string from utf8");
    let offer =
        serde_json::from_str::<RTCSessionDescription>(&desc_data).expect("couldn't deserialize");

    peer_connection
        .set_remote_description(offer)
        .await
        .expect("couldn't set remote description");

    let answer = peer_connection
        .create_answer(None)
        .await
        .expect("couldn't create answer");

    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    peer_connection
        .set_local_description(answer)
        .await
        .expect("couldn't set local description");

    let _ = gather_complete.recv().await;

    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)
            .expect("couldn't deserialize local description to string");
        let b64 = base64::encode(&json_str);
        println!("{}", json_str);
        println!("{}", b64);
    } else {
        println!("generate local_description failed!");
    }

    let listener = UdpSocket::bind("127.0.0.1:5004")
        .await
        .expect("couldn't bind to local udp socket");

    let done_tx3 = done_tx.clone();

    spawn!(async move {
        let mut inbound_rtp_packet = vec![0u8; 1600]; // UDP MTU
        while let Ok((n, _)) = listener.recv_from(&mut inbound_rtp_packet).await {
            if let Err(err) = video_track.write(&inbound_rtp_packet[..n]).await {
                if Error::ErrClosedPipe == err {
                    // The peerConnection has been closed.
                } else {
                    println!("video_track write err: {}", err);
                }
                let _ = done_tx3.try_send(());
                return;
            }
        }
    });

    println!("Press ctrl-c to stop");
    select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
    };

    peer_connection
        .close()
        .await
        .expect("couldn't close connection");

    Ok(())
}

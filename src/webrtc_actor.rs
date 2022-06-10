use std::sync::Arc;

use bastion::{
    spawn,
    supervisor::{RestartPolicy, RestartStrategy, SupervisorRef}, distributor::Distributor, context::BastionContext, message::MessageHandler, blocking, run,
};
use tokio::{net::UdpSocket, select};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_H264},
        APIBuilder,
    },
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer, ice_candidate::{RTCIceCandidate, RTCIceCandidateInit}},
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

use crate::{gstreamer_actor::GstreamerActor, webrtcbin_actor::SDPType};

pub struct WebRtcActor;

impl WebRtcActor {
    pub fn run(parent: SupervisorRef, sdp: &str, i: u8) {
        let sdp = sdp.to_owned();
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never), // .with_restart_policy(RestartPolicy::Tries(5))
                                                                                          // .with_actor_restart_strategy(ActorRestartStrategy::Immediate),
                )
                .children(|c| {
                    c.with_distributor(Distributor::named(format!("webrtc_{i}")))
                    .with_exec(move |ctx| {
                        println!("WebRTC started");
                        let sdp = sdp.clone();
                        GstreamerActor::run(ctx.supervisor().unwrap().supervisor(|s| s).unwrap(), i);
                        main_fn(sdp, i, ctx)
                    })
                })
            })
            .expect("couldn't run WebRTC actor");
    }
}

async fn main_fn(sdp: String, i: u8, ctx: BastionContext) -> Result<(), ()> {
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
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .expect("couldn't create new peer connection"),
    );

    let pc = Arc::downgrade(&peer_connection);
    blocking!(async move {
        loop {
            let pc = pc.clone();
            MessageHandler::new(ctx.recv().await.unwrap()).on_tell(|candidate: String, _| {
                println!("RECEIVED: {candidate}");
                run!(async {
                    let candidate = serde_json::from_str::<RTCIceCandidateInit>(&candidate).unwrap();
                    if let Some(pc) = pc.upgrade() {
                        pc.add_ice_candidate(candidate).await.unwrap();
                }});
            });
        }
    });

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

    let pc = Arc::downgrade(&peer_connection);
    peer_connection.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let pc = pc.clone();
        Box::pin(async move {
            if let Some(c) = c {
                if let Some(pc) = pc.upgrade() {
                    let desc = pc.remote_description().await;
                    if desc.is_some() {
                        let candidate = c.to_json().await.unwrap();
                        let mline_index = candidate.sdp_mline_index;
                        let sdp_mid = candidate.sdp_mid;
                        let candidate = candidate.candidate;
                        println!("send ice: {candidate}");
                        Distributor::named(format!("web_socket_{i}")).tell_one((mline_index, candidate, sdp_mid)).expect("couldn't send ICE to peer");
                    }
                }
            }
        })
    })).await;

    println!("received: {sdp}");
    let offer =
        serde_json::from_str::<RTCSessionDescription>(&sdp).expect("couldn't deserialize");

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
        let json_str = local_desc.sdp;
        Distributor::named(format!("web_socket_{i}")).tell_one((SDPType::Answer, json_str)).expect("couldn't send SDP a to peer");
    } else {
        println!("generate local_description failed!");
    }

    let listener = UdpSocket::bind(format!("127.0.0.1:500{i}"))
        .await
        .expect("couldn't bind to local udp socket");

    let done_tx3 = done_tx.clone();

    let handler = spawn!(async move {
        let mut inbound_rtp_packet = vec![0u8; 1600]; // UDP MTU
        println!("cho nhan data neeeeee");
        while let Ok((n, _)) = listener.recv_from(&mut inbound_rtp_packet).await {
            // println!("data neeeee {i}{i}{i}{i}");
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
            println!("received done signal! {i}");
        }
    };

    handler.cancel();

    peer_connection
        .close()
        .await
        .expect("couldn't close connection");

    Ok(())
}

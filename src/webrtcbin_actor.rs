use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, Weak},
};

use anyhow::{bail, Context};
use bastion::{
    blocking,
    context::BastionContext,
    distributor::Distributor,
    message::MessageHandler,
    run,
    supervisor::{ActorRestartStrategy, RestartPolicy, RestartStrategy, SupervisorRef},
};
use byte_slice_cast::AsSliceOf;
use gst::{
    glib,
    prelude::{Cast, ElementExtManual, GObjectExtManualGst, ObjectExt, PadExtManual, ToValue},
    traits::{ElementExt, GstBinExt, GstObjectExt, PadExt},
};
use gst_sdp::SDPMessage;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{upgrade_weak, utils};

pub type SDPType = gst_webrtc::WebRTCSDPType;
pub type SessionDescription = gst_webrtc::WebRTCSessionDescription;

const VIDEO_WIDTH: u32 = 1280;
const VIDEO_HEIGHT: u32 = 720;

#[derive(Copy, Clone)]
pub enum WebRTCBinActorType {
    Client,
    Server,
}

impl AsRef<str> for WebRTCBinActorType {
    fn as_ref(&self) -> &str {
        match self {
            &Self::Client => "client",
            &Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone)]
struct Peer(Arc<PeerInner>);

#[derive(Debug, Clone)]
struct PeerWeak(Weak<PeerInner>);

#[derive(Debug)]
struct PeerInner {
    id: u32,
    bin: gst::Bin,
    webrtcbin: gst::Element,
}

impl std::ops::Deref for Peer {
    type Target = PeerInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PeerWeak {
    fn upgrade(&self) -> Option<Peer> {
        self.0.upgrade().map(Peer)
    }
}

impl Peer {
    pub fn downgrade(&self) -> PeerWeak {
        PeerWeak(Arc::downgrade(&self.0))
    }
}

impl Peer {
    async fn handle_sdp(
        &self,
        type_: SDPType,
        sdp: SDPMessage,
        order: u8,
    ) -> Result<(), anyhow::Error> {
        match type_ {
            SDPType::Answer => {
                let answer = SessionDescription::new(SDPType::Answer, sdp);

                self.webrtcbin
                    .emit_by_name("set-remote-description", &[&answer, &None::<gst::Promise>])
                    .expect("couldn't set remote description for webrtcbin");

                Ok(())
            }
            SDPType::Offer => {
                let peer_weak = self.downgrade();
                self.bin.call_async(move |_| {
                    let peer = upgrade_weak!(peer_weak);

                    let offer = SessionDescription::new(type_, sdp);
                    peer.0
                        .webrtcbin
                        .emit_by_name("set-remote-description", &[&offer, &None::<gst::Promise>])
                        .expect("couldn't set remote description for webrtcbin");

                    let peer_cl = peer.downgrade();
                    let promise = gst::Promise::with_change_func(move |reply| {
                        let peer = upgrade_weak!(peer_cl);

                        if let Err(err) = peer.on_answer_created(reply, order) {
                            gst::element_error!(
                                peer.bin,
                                gst::LibraryError::Failed,
                                ("Failed to send SDP answer: {:?}", err)
                            );
                        }
                    });

                    peer.0
                        .webrtcbin
                        .emit_by_name("create-answer", &[&None::<gst::Structure>, &promise])
                        .expect("couldn't create answer for webrtcbin");
                });

                Ok(())
            }
            _ => bail!("SDP type is not \"answer\" but \"{}\"", type_.to_str()),
        }
    }

    fn handle_ice(
        &self,
        mlineindex: u32,
        candidate: String,
        order: u8,
    ) -> Result<(), anyhow::Error> {
        self.webrtcbin
            .emit_by_name("add-ice-candidate", &[&mlineindex, &candidate])
            .expect("couldn't add ice candidate");
        Ok(())
    }

    fn on_answer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
        order: u8,
    ) -> Result<(), anyhow::Error> {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                bail!("Answer creation future got no response");
            }
            Err(err) => {
                bail!("Answer creation future got error response: {:?}", err);
            }
        };

        let answer = reply
            .value("answer")
            .unwrap()
            .get::<SessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&answer, &None::<gst::Promise>])
            .expect("couldn't set local description for webrtcbin");

        let sdp = answer.sdp();

        Distributor::named(format!("web_socket_{}", order))
            .tell_one((SDPType::Answer, sdp))
            .expect("couldn't send SDP answer to client");

        Ok(())
    }

    fn on_ice_candidate(
        &self,
        type_: &str,
        mlineindex: u32,
        candidate: String,
    ) -> Result<(), anyhow::Error> {
        Distributor::named(type_)
            .tell_one((mlineindex, candidate))
            .expect("couldn't send msg");
        Ok(())
    }

    fn on_incoming_stream(&self, pad: &gst::Pad) -> Result<(), anyhow::Error> {
        // Early return for the source pads we're adding ourselves
        if pad.direction() != gst::PadDirection::Src {
            return Ok(());
        }

        let caps = pad.current_caps().unwrap();
        let s = caps.structure(0).unwrap();
        let media_type = s
            .get::<&str>("media")
            .map_err(|_| anyhow::anyhow!("no media type in caps: {caps:?}"))?;

        let conv = if media_type == "video" {
            gst::parse_bin_from_description(&format!("
            decodebin name=dbin ! queue ! videoconvert ! videoscale ! capsfilter name=src caps=video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1
            ", width=VIDEO_WIDTH, height=VIDEO_HEIGHT), false)?
        } else {
            println!("Unknown pad {pad:?}, ignoring");
            return Ok(());
        };

        let dbin = conv.by_name("dbin").unwrap();
        let sink_pad =
            gst::GhostPad::with_target(Some("sink"), &dbin.static_pad("sink").unwrap()).unwrap();
        conv.add_pad(&sink_pad).unwrap();

        let src = conv.by_name("src").unwrap();
        let src_pad =
            gst::GhostPad::with_target(Some("src"), &src.static_pad("src").unwrap()).unwrap();
        conv.add_pad(&src_pad).unwrap();

        self.bin.add(&conv).unwrap();
        conv.sync_state_with_parent()
            .with_context(|| format!("can't start sink for stream {caps:?}"))?;

        pad.link(&sink_pad)
            .with_context(|| format!("can't link sink for stream {caps:?}"))?;

        if media_type == "video" {
            let src_pad = gst::GhostPad::with_target(Some("video_src"), &src_pad).unwrap();
            src_pad.set_active(true).unwrap();
            self.bin.add_pad(&src_pad).unwrap();
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct WebRTCPipeline(Arc<WebRTCPipelineInner>);

#[derive(Debug, Clone)]
pub struct WebRTCPipelineWeak(Weak<WebRTCPipelineInner>);

#[derive(Debug)]
pub struct WebRTCPipelineInner {
    pipeline: gst::Pipeline,
    video_tee: gst::Element,
    peers: Mutex<BTreeMap<u32, Peer>>,
}

impl std::ops::Deref for WebRTCPipeline {
    type Target = WebRTCPipelineInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for WebRTCPipelineInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl WebRTCPipelineWeak {
    fn upgrade(&self) -> Option<WebRTCPipeline> {
        self.0.upgrade().map(WebRTCPipeline)
    }
}

impl WebRTCPipeline {
    pub fn downgrade(&self) -> WebRTCPipelineWeak {
        WebRTCPipelineWeak(Arc::downgrade(&self.0))
    }
}

impl WebRTCPipeline {
    fn create_server(order: u8) -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch(
            &format!("videotestsrc pattern=ball is-live=true ! videoconvert ! queue max-size-buffers=1 !
            x264enc bitrate=600 speed-preset=ultrafast tune=zerolatency key-int-max=15 ! video/x-h264,profile=constrained-baseline ! queue max-size-time=100000000 ! h264parse !
            rtph264pay config-interval=-1 aggregate-mode=zero-latency ! application/x-rtp,media=video,encoding-name=H264,payload=96 ! tee name=video-tee ! queue ! fakesink sync=true")
        )
        .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let video_tee = pipeline.by_name("video-tee").expect("video-tee not found");

        let pipeline = Self(Arc::new(WebRTCPipelineInner {
            pipeline,
            video_tee,
            peers: Mutex::new(BTreeMap::new()),
        }));

        Ok(pipeline)
    }

    pub fn init(type_: &WebRTCBinActorType, order: u8) -> Result<Self, anyhow::Error> {
        Self::create_server(order)
    }

    pub fn run(&self) -> Result<(), anyhow::Error> {
        self.pipeline.call_async(|pipeline| {
            if pipeline.set_state(gst::State::Playing).is_err() {
                gst::element_error!(
                    pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to set pipeline to Playing")
                );
            }
        });

        Ok(())
    }

    async fn add_peer(&self, peer_id: u32, order: u8) -> Result<(), anyhow::Error> {
        println!("Adding peer {peer_id}..");

        let mut peers = self.peers.lock().await;
        if peers.contains_key(&peer_id) {
            bail!("Peer {peer_id} already connected");
        }

        let peer_bin = gst::parse_bin_from_description(
            "
            queue name=video_queue ! webrtcbin. \
            webrtcbin name=webrtcbin bundle-policy=max-bundile \
            turn-server=turn://tel4vn:TEL4VN.COM@turn.tel4vn.com:5349?transport=tcp
        ",
            false,
        )?;

        let webrtcbin = peer_bin.by_name("webrtcbin").expect("webrtcbin not found");
        if let Some(transceiver) = webrtcbin
            .emit_by_name("get-transceiver", &[&0.to_value()])
            .unwrap()
            .and_then(|val| val.get::<gst_webrtc::WebRTCRTPTransceiver>().ok())
        {
            transceiver.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            )?;
        }

        let video_queue = peer_bin
            .by_name("video-queue")
            .expect("video-queue not found");
        let video_sink_pad = gst::GhostPad::with_target(
            Some("video_sink"),
            &video_queue.static_pad("sink").unwrap(),
        )
        .unwrap();

        peer_bin.add_pad(&video_sink_pad).unwrap();

        let peer = Peer(Arc::new(PeerInner {
            id: peer_id,
            bin: peer_bin,
            webrtcbin,
        }));

        peers.insert(peer_id, peer.clone());
        drop(peers);

        self.pipeline.add(&peer.bin).unwrap();

        let peer_cl = peer.downgrade();
        peer.webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                let mlineindex = values[1].get::<u32>().expect("invalid argument");
                let candidate = values[2].get::<String>().expect("invalid argument");

                let peer = upgrade_weak!(peer_cl, None);

                if let Err(err) =
                    peer.on_ice_candidate(&format!("web_socket_{}", order), mlineindex, candidate)
                {
                    gst::element_error!(
                        peer.bin,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {:?}", err)
                    );
                }

                None
            })
            .expect("couldn't connect webrtcbin to ice candidate process");

        let peer_cl = peer.downgrade();
        peer.webrtcbin.connect_pad_added(move |_, pad| {
            let peer = upgrade_weak!(peer_cl);

            if let Err(err) = peer.on_incoming_stream(pad) {
                gst::element_error!(
                    peer.bin,
                    gst::LibraryError::Failed,
                    ("Failed to handle incoming stream: {:?}", err)
                );
            }
        });

        let video_src_pad = self.video_tee.request_pad_simple("src_%u").unwrap();
        let video_block = video_src_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        video_src_pad.link(&video_sink_pad)?;

        peer.bin.call_async(move |bin| {
            if bin.sync_state_with_parent().is_err() {
                gst::element_error!(
                    bin,
                    gst::LibraryError::Failed,
                    ("Failed to set peer bin to playing")
                );
            }

            video_src_pad.remove_probe(video_block);
        });

        Ok(())
    }

    async fn remove_peer(&self, peer_id: u32, order: u8) -> Result<(), anyhow::Error> {
        println!("Removing peer {peer_id}..");

        let mut peers = self.peers.lock().await;
        if let Some(peer) = peers.remove(&peer_id) {
            drop(peers);

            let pipeline_cl = self.downgrade();
            self.pipeline.call_async(move |_| {
                let pipeline = upgrade_weak!(pipeline_cl);

                let videotee_sink_pad = pipeline.video_tee.static_pad("sink").unwrap();
                let video_block = videotee_sink_pad
                    .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
                        gst::PadProbeReturn::Ok
                    })
                    .unwrap();

                let video_sink_pad = peer.bin.static_pad("video_sink").unwrap();

                if let Some(videotee_src_pad) = video_sink_pad.peer() {
                    let _ = videotee_src_pad.unlink(&video_sink_pad);
                    pipeline.video_tee.release_request_pad(&videotee_src_pad);
                }
                videotee_sink_pad.remove_probe(video_block);

                let _ = pipeline.pipeline.remove(&peer.bin);
                let _ = peer.bin.set_state(gst::State::Null);

                println!("Removed peer {}", peer.id);
            })
        }

        Ok(())
    }
}

fn main_loop(pipeline: WebRTCPipeline) -> Result<(), anyhow::Error> {
    let bus = pipeline.pipeline.bus().unwrap();

    println!("Main loop running..!!!");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::message::MessageView;
        match msg.view() {
            MessageView::Error(err) => bail!(
                "Error from element {}: {} ({})",
                err.src()
                    .map(|s| String::from(s.path_string()))
                    .unwrap_or_else(|| String::from("None")),
                err.error(),
                err.debug().unwrap_or_else(|| String::from("None")),
            ),
            MessageView::Warning(warning) => {
                println!("Warning: \"{}\"", warning.debug().unwrap());
            }
            MessageView::Eos(..) => {
                println!(" ========================================= ");
                return Ok(());
            }
            MessageView::Element(elm) => {
                println!("Element: {:?}", elm);
            }
            _ => (),
        }
    }
    println!("Break");
    Ok(())
}

pub struct WebRTCBinActor;

impl WebRTCBinActor {
    pub fn run(parent: SupervisorRef, type_: WebRTCBinActorType, order: u8) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never),
                )
                .children(move |c| {
                    c.with_distributor(Distributor::named(format!("{}_{}", type_.as_ref(), order)))
                        .with_exec(move |ctx| main_fn(ctx, type_, order))
                })
            })
            .expect("couldn't run Gstreamer actor");
    }
}

async fn main_fn(ctx: BastionContext, type_: WebRTCBinActorType, order: u8) -> Result<(), ()> {
    println!("WebRTCBin {}_{} started", type_.as_ref(), order);
    gst::init().expect("couldn't initialize gstreamer");
    let pipeline = WebRTCPipeline::init(&type_, order).expect("couldn't create webrtcbin pipeline");
    pipeline.run().expect("couldn't start webrtc pipeline up");
    let pl_clone = pipeline.downgrade();
    // blocking! {main_loop(pipeline)};
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(
                |(peer_id, (sdp_type, sdp)): (u32, (SDPType, SDPMessage)), _| {
                    let pipeline = upgrade_weak!(pl_clone);
                    run!(async {
                        let peers = pipeline.peers.lock().await;
                        let peer = peers
                            .get(&peer_id)
                            .ok_or_else(|| anyhow::anyhow!("can't find peer {peer_id}"))
                            .unwrap()
                            .clone();
                        drop(peers);
                        peer.handle_sdp(sdp_type, sdp, order).await;
                    });
                },
            )
            .on_tell(|(peer_id, ice_candidate): (u32, (u32, String)), _| {
                let pipeline = upgrade_weak!(pl_clone);
                run!(async {
                    let peers = pipeline.peers.lock().await;
                    let peer = peers
                        .get(&peer_id)
                        .ok_or_else(|| anyhow::anyhow!("can't find peer {peer_id}"))
                        .unwrap()
                        .clone();
                    drop(peers);
                    peer.handle_ice(ice_candidate.0, ice_candidate.1, order);
                });
            })
            .on_tell(|(msg_type, peer_id): (&str, String), _| {
                let pipeline = upgrade_weak!(pl_clone);
                let peer_id = str::parse::<u32>(&peer_id)
                    .with_context(|| format!("Can't parse peer id"))
                    .unwrap();
                run!(async {
                    match msg_type {
                        "add" => {
                            pipeline.add_peer(peer_id, order).await;
                        }
                        "remove" => {
                            pipeline.remove_peer(peer_id, order).await;
                        }
                        _ => {}
                    }
                });
            });
    }
}

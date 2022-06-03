use std::{
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
    prelude::{Cast, ElementExtManual, GObjectExtManualGst, ObjectExt, ToValue},
    traits::{ElementExt, GstBinExt, GstObjectExt, PadExt},
};
use gst_sdp::SDPMessage;
use serde_json::{json, Value};

use crate::{upgrade_weak, utils};

pub type SDPType = gst_webrtc::WebRTCSDPType;
pub type SessionDescription = gst_webrtc::WebRTCSessionDescription;

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
pub struct WebRTCPipeline(Arc<WebRTCPipelineInner>);

#[derive(Debug, Clone)]
pub struct WebRTCPipelineWeak(Weak<WebRTCPipelineInner>);

#[derive(Debug)]
pub struct WebRTCPipelineInner {
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
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
    fn create_client(order: u8) -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch("webrtcbin name=webrtcbin ! audiotestsrc ! fakesink")
            .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");
        webrtcbin.set_property_from_str("stun-server", "stun://stun.l.google.com:19302");
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        let direction = gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly;
        let caps = gst::Caps::from_str(
            "application/x-rtp,media=video,encoding-name=VP8,payload=96,clock-rate=90000",
        )?;
        webrtcbin
            .emit_by_name("add-transceiver", &[&direction, &caps])
            .expect("couldn't add transceiver to pipeline");

        let pipeline = Self(Arc::new(WebRTCPipelineInner {
            pipeline,
            webrtcbin,
        }));

        let pl_clone = pipeline.downgrade();
        pipeline
            .webrtcbin
            .connect("on-negotiation-needed", true, move |_| {
                let pipeline = upgrade_weak!(pl_clone, None);
                if let Err(err) = pipeline.on_negotiation_needed(order) {
                    gst::element_error!(
                        pipeline.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to negotiate: {:?}", err)
                    );
                }

                None
            })?;

        let pl_clone = pipeline.downgrade();
        pipeline
            .webrtcbin
            .connect("on-ice-candidate", true, move |values| {
                let mlineindex = values[1].get::<u32>().expect("invalid argument");
                let candidate = values[2].get::<String>().expect("invalid argument");
                let pipeline = upgrade_weak!(pl_clone, None);

                if let Err(err) = pipeline.on_ice_candidate("server", mlineindex, candidate) {
                    gst::element_error!(
                        pipeline.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {:?}", err)
                    );
                }

                None
            })
            .expect("couldn't connect webrtcbin to ice candidate process");

        let pl_clone = pipeline.downgrade();
        pipeline.webrtcbin.connect_pad_added(move |_, pad| {
            let pipeline = upgrade_weak!(pl_clone);

            if let Err(err) = pipeline.on_incoming_stream(pad) {
                gst::element_error!(
                    pipeline.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle incoming stream: {:?}", err)
                );
            }
        });

        Ok(pipeline)
    }

    fn create_server(order: u8) -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch(
            "webrtcbin name=webrtcbin message-forward=true turn-server=turn://tel4vn:TEL4VN.COM@turn.tel4vn.com:5349?transport=tcp bundle-policy=max-bundle 
            videotestsrc pattern=ball is-live=true ! video/x-raw,width=1280,height=720 ! videoconvert ! 
            x264enc threads=4 bitrate=600 speed-preset=ultrafast tune=zerolatency key-int-max=15 ! 
            video/x-h264,profile=constrained-baseline ! h264parse ! rtph264pay config-interval=1 ! 
            application/x-rtp,media=video,encoding-name=H264,payload=100,clock-rate=90000,aggregate-mode=zero-latency,profile-level-id=42e01f ! webrtcbin.",
        )
        .expect("couldn't parse pipeline from string");
        // let pipeline = gst::parse_launch(
        //     "webrtcbin name=webrtcbin rtspsrc location=rtsp://test:test123@192.168.1.11:88/videoMain is-live=true !
        //     application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000 ! webrtcbin.",
        // )
        // .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");

        // if let Some(transceiver) = webrtcbin
        //     .emit_by_name("get-transceiver", &[&0.to_value()])
        //     .unwrap()
        //     .and_then(|val| val.get::<gst_webrtc::WebRTCRTPTransceiver>().ok())
        // {
        //     transceiver.set_property("do-nack", &false.to_value())?;
        //     transceiver.set_property(
        //         "direction",
        //         (gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly).to_value(),
        //     )?;
        // }

        let pipeline = Self(Arc::new(WebRTCPipelineInner {
            pipeline,
            webrtcbin,
        }));

        let pl_clone = pipeline.downgrade();
        pipeline
            .webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                let mlineindex = values[1].get::<u32>().expect("invalid argument");
                let candidate = values[2].get::<String>().expect("invalid argument");

                let pipeline = upgrade_weak!(pl_clone, None);

                if let Err(err) = pipeline.on_ice_candidate(
                    &format!("web_socket_{}", order),
                    mlineindex,
                    candidate,
                ) {
                    gst::element_error!(
                        pipeline.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {:?}", err)
                    );
                }

                None
            })
            .expect("couldn't connect webrtcbin to ice candidate process");

        Ok(pipeline)
    }

    pub fn init(type_: &WebRTCBinActorType, order: u8) -> Result<Self, anyhow::Error> {
        match type_ {
            &WebRTCBinActorType::Server => Self::create_server(order),
            &WebRTCBinActorType::Client => Self::create_client(order),
        }
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
                let pl_clone = self.downgrade();
                self.pipeline.call_async(move |_| {
                    let pipeline = upgrade_weak!(pl_clone);

                    let offer = SessionDescription::new(type_, sdp);
                    pipeline
                        .0
                        .webrtcbin
                        .emit_by_name("set-remote-description", &[&offer, &None::<gst::Promise>])
                        .expect("couldn't set remote description for webrtcbin");

                    let pl_clone = pipeline.downgrade();
                    let promise = gst::Promise::with_change_func(move |reply| {
                        let pipeline = upgrade_weak!(pl_clone);

                        if let Err(err) = pipeline.on_answer_created(reply, order) {
                            gst::element_error!(
                                pipeline.pipeline,
                                gst::LibraryError::Failed,
                                ("Failed to send SDP answer: {:?}", err)
                            );
                        }
                    });

                    pipeline
                        .0
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
        // let property = self.webrtcbin.property("ice-connection-state")?;
        // let ice_state = property.get::<gst_webrtc::WebRTCICEConnectionState>()?;
        // println!("\n==========           ice state: {:?}\n", ice_state);
        Ok(())
    }

    fn on_negotiation_needed(&self, order: u8) -> Result<(), anyhow::Error> {
        println!("Starting negotiation");

        let pl_clone = self.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let pipeline = upgrade_weak!(pl_clone);

            if let Err(err) = pipeline.on_offer_created(reply, order) {
                gst::element_error!(
                    pipeline.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to send SDP offer: {:?}", err)
                );
            }
        });

        self.webrtcbin
            .emit_by_name("create-offer", &[&None::<gst::Structure>, &promise])
            .expect("couldn't create offer");

        Ok(())
    }

    fn on_offer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
        order: u8,
    ) -> Result<(), anyhow::Error> {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                bail!("Offer creation future got no response");
            }
            Err(err) => {
                bail!("Offer creation future got error response: {:?}", err);
            }
        };

        let offer = reply
            .value("offer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&offer, &None::<gst::Promise>])
            .unwrap();

        let sdp = offer.sdp();

        Distributor::named(format!("server_{}", order))
            .tell_one((SDPType::Offer, sdp))
            .expect("couldn't send SDP offer to server");

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

        let decodebin = gst::ElementFactory::make("decodebin", None).unwrap();
        let app_clone = self.downgrade();
        decodebin.connect_pad_added(move |_decodebin, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_decodebin_stream(pad) {
                gst::element_error!(
                    app.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle decoded stream: {:?}", err)
                );
            }
        });

        self.pipeline.add(&decodebin).unwrap();
        decodebin.sync_state_with_parent().unwrap();

        let sinkpad = decodebin.static_pad("sink").unwrap();
        pad.link(&sinkpad);

        Ok(())
    }

    fn on_incoming_decodebin_stream(&self, pad: &gst::Pad) -> Result<(), anyhow::Error> {
        let caps = pad.current_caps().unwrap();
        let name = caps.structure(0).unwrap().name();

        let sink = if name.starts_with("video/") {
            // gst::parse_bin_from_description("queue ! videoconvert ! appsink name=app", true)?
            gst::parse_bin_from_description("queue ! videoconvert ! jpegenc ! multifilesink post-messages=true location=\"./frames/frame%d.jpg\"", true)?
        } else {
            println!("Unknown pad {:?}, ignoring", pad);
            return Ok(());
        };

        // let app = sink
        //     .by_name("app")
        //     .expect("couldn't get element named app")
        //     .downcast::<gst_app::AppSink>()
        //     .expect("couldn't downcast to appsink");

        // app.set_property("max-buffers", 5u32);
        // app.set_property("drop", true);
        // app.set_property("sync", true);
        // app.set_property("wait-on-eos", false);

        // app.set_callbacks(
        //     gst_app::AppSinkCallbacks::builder()
        //         .new_sample(move |appsink| {
        //             let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;

        //             println!("{:?}", sample);

        //             Ok(gst::FlowSuccess::Ok)
        //         })
        //         .build(),
        // );

        self.pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent()
            .with_context(|| format!("can't start sink for stream {:?}", caps))?;

        let sinkpad = sink.static_pad("sink").unwrap();
        pad.link(&sinkpad)
            .with_context(|| format!("can't link sink for stream {:?}", caps))?;

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
    blocking! {main_loop(pipeline)};
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|(sdp_type, sdp): (SDPType, SDPMessage), _| {
                println!("{} RECEIVED:\n{}", type_.as_ref(), sdp.as_text().unwrap());
                run! { async {
                    let pipeline = upgrade_weak!(pl_clone);
                    pipeline
                        .handle_sdp(sdp_type, sdp, order)
                        .await
                        .expect("couldn't handle sdp");
                }}
            })
            .on_tell(|ice_candidate: (u32, String), _| {
                println!("{} RECEIVED:\n{:?}", type_.as_ref(), ice_candidate);
                let pipeline = upgrade_weak!(pl_clone);
                pipeline
                    .handle_ice(ice_candidate.0, ice_candidate.1, order)
                    .expect("couldn't handle sdp");
            });
    }
}

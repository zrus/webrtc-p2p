use std::sync::{Arc, Mutex, Weak};

use bastion::context::BastionContext;
use bastion::distributor::Distributor;
use bastion::message::MessageHandler;
use bastion::supervisor::SupervisorRef;
use gst::element_error;
use gst::prelude::*;

use gst_sdp::SDPMessage;
use serde_derive::{Deserialize, Serialize};

use anyhow::{anyhow, bail, Context};

use crate::upgrade_weak;
use crate::utils;
use crate::webrtcbin_actor::SDPType;
use crate::webrtcbin_actor::SessionDescription;
use crate::webrtcbin_actor::WebRTCBinActorType;

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

// JSON messages we communicate with
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

// Strong reference to our application state
#[derive(Debug, Clone)]
struct App(Arc<AppInner>);

// Weak reference to our application state
#[derive(Debug, Clone)]
struct AppWeak(Weak<AppInner>);

// Actual application state
#[derive(Debug)]
struct AppInner {
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
}

// To be able to access the App's fields directly
impl std::ops::Deref for App {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.0
    }
}

impl AppWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(App)
    }
}

impl App {
    // Downgrade the strong reference to a weak reference
    fn downgrade(&self) -> AppWeak {
        AppWeak(Arc::downgrade(&self.0))
    }

    fn new(type_: WebRTCBinActorType) -> Result<Self, anyhow::Error> {
        // Create the GStreamer pipeline
        let pipeline = gst::parse_launch(
        "videotestsrc pattern=ball is-live=true ! vp8enc deadline=1 ! rtpvp8pay pt=96 ! webrtcbin. \
         webrtcbin name=webrtcbin"
    )?;

        // Downcast from gst::Element to gst::Pipeline
        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("not a pipeline");

        // Get access to the webrtcbin by name
        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");

        // Set some properties on webrtcbin
        webrtcbin.set_property_from_str("stun-server", STUN_SERVER);
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        // Create a stream for handling the GStreamer message asynchronously
        let bus = pipeline.bus().unwrap();
        let send_gst_msg_rx = bus.stream();

        let app = App(Arc::new(AppInner {
            pipeline,
            webrtcbin,
        }));

        // Connect to on-negotiation-needed to handle sending an Offer
        if type_.as_ref() == "client" {
            let app_clone = app.downgrade();
            app.webrtcbin
                .connect("on-negotiation-needed", false, move |values| {
                    let app = upgrade_weak!(app_clone, None);
                    if let Err(err) = app.on_negotiation_needed() {
                        element_error!(
                            app.pipeline,
                            gst::LibraryError::Failed,
                            ("Failed to negotiate: {:?}", err)
                        );
                    }

                    None
                })
                .unwrap();
        }

        // Whenever there is a new ICE candidate, send it to the peer
        let app_clone = app.downgrade();
        app.webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                let _webrtc = values[0].get::<gst::Element>().expect("Invalid argument");
                let mlineindex = values[1].get::<u32>().expect("Invalid argument");
                let candidate = values[2].get::<String>().expect("Invalid argument");

                let app = upgrade_weak!(app_clone, None);

                if let Err(err) = app.on_ice_candidate(type_.as_ref(), mlineindex, candidate) {
                    element_error!(
                        app.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {:?}", err)
                    );
                }

                None
            })
            .unwrap();

        // Whenever there is a new stream incoming from the peer, handle it
        let app_clone = app.downgrade();
        app.webrtcbin.connect_pad_added(move |_webrtc, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_stream(pad) {
                element_error!(
                    app.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle incoming stream: {:?}", err)
                );
            }
        });

        // Asynchronously set the pipeline to Playing
        app.pipeline.call_async(|pipeline| {
            // If this fails, post an error on the bus so we exit
            if pipeline.set_state(gst::State::Playing).is_err() {
                element_error!(
                    pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to set pipeline to Playing")
                );
            }
        });

        // Asynchronously set the pipeline to Playing
        app.pipeline.call_async(|pipeline| {
            pipeline
                .set_state(gst::State::Playing)
                .expect("Couldn't set pipeline to Playing");
        });

        Ok(app)
    }

    // Whenever webrtcbin tells us that (re-)negotiation is needed, simply ask
    // for a new offer SDP from webrtcbin without any customization and then
    // asynchronously send it to the peer via the WebSocket connection
    fn on_negotiation_needed(&self) -> Result<(), anyhow::Error> {
        println!("starting negotiation");

        let app_clone = self.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_offer_created(reply) {
                element_error!(
                    app.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to send SDP offer: {:?}", err)
                );
            }
        });

        self.webrtcbin
            .emit_by_name("create-offer", &[&None::<gst::Structure>, &promise])
            .unwrap();

        Ok(())
    }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_offer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
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

        let mut sdp = offer.sdp();

        Distributor::named("server")
            .tell_one((SDPType::Offer, sdp))
            .expect("couldn't send SDP offer to server");

        Ok(())
    }

    // Once webrtcbin has create the answer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_answer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
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
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&answer, &None::<gst::Promise>])
            .unwrap();

        let sdp = answer.sdp();

        Distributor::named("client")
            .tell_one((SDPType::Answer, sdp))
            .expect("couldn't send SDP answer to client");

        Ok(())
    }

    // Handle incoming SDP answers from the peer
    fn handle_sdp(&self, type_: SDPType, sdp: SDPMessage) -> Result<(), anyhow::Error> {
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

                        if let Err(err) = pipeline.on_answer_created(reply) {
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

    // Handle incoming ICE candidates from the peer by passing them to webrtcbin
    fn handle_ice(&self, sdp_mline_index: u32, candidate: &str) -> Result<(), anyhow::Error> {
        self.webrtcbin
            .emit_by_name("add-ice-candidate", &[&sdp_mline_index, &candidate])
            .expect("couldn't add ice candidate");
        let property = self.webrtcbin.property("ice-connection-state")?;
        let ice_state = property.get::<gst_webrtc::WebRTCICEConnectionState>()?;
        println!("\n==========           ice state: {:?}\n", ice_state);
        Ok(())
    }

    // Asynchronously send ICE candidates to the peer via the WebSocket connection as a JSON
    // message
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

    // Whenever there's a new incoming, encoded stream from the peer create a new decodebin
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
                element_error!(
                    app.pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle decoded stream: {:?}", err)
                );
            }
        });

        self.pipeline.add(&decodebin).unwrap();
        decodebin.sync_state_with_parent().unwrap();

        let sinkpad = decodebin.static_pad("sink").unwrap();
        pad.link(&sinkpad).unwrap();

        Ok(())
    }

    // Handle a newly decoded decodebin stream and depending on its type, create the relevant
    // elements or simply ignore it
    fn on_incoming_decodebin_stream(&self, pad: &gst::Pad) -> Result<(), anyhow::Error> {
        let caps = pad.current_caps().unwrap();
        let name = caps.structure(0).unwrap().name();

        let sink = if name.starts_with("video/") {
            gst::parse_bin_from_description(
                "queue ! videoconvert ! videoscale ! autovideosink",
                true,
            )?
        } else {
            println!("Unknown pad {:?}, ignoring", pad);
            return Ok(());
        };

        self.pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent()
            .with_context(|| format!("can't start sink for stream {:?}", caps))?;

        let sinkpad = sink.static_pad("sink").unwrap();
        pad.link(&sinkpad)
            .with_context(|| format!("can't link sink for stream {:?}", caps))?;

        Ok(())
    }
}

// Make sure to shut down the pipeline when it goes out of scope
// to release any system resources
impl Drop for AppInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn main_loop(pipeline: App) -> Result<(), anyhow::Error> {
    let bus = pipeline.pipeline.bus().unwrap();

    println!("HELLOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOO");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::message::MessageView;
        println!("============================");
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
            MessageView::Eos(..) => return Ok(()),
            _ => (),
        }
    }
    println!("Break");
    Ok(())
}

async fn run(ctx: BastionContext, type_: WebRTCBinActorType) -> Result<(), ()> {
    gst::init().expect("");
    let app = App::new(type_).unwrap();
    let app_clone = app.downgrade();
    bastion::blocking! {main_loop(app)};
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|(sdp_type, sdp): (SDPType, SDPMessage), _| {
                println!(
                    "{} sdp received: {}",
                    type_.as_ref(),
                    sdp.as_text().unwrap()
                );
                bastion::run! { async {
                        let app = upgrade_weak!(app_clone);
                        app
                            .handle_sdp(sdp_type, sdp);
                    }
                }
            })
            .on_tell(|ice_candidate: (u32, String), _| {
                println!("{} candidate received: {:?}", type_.as_ref(), ice_candidate);
                let app = upgrade_weak!(app_clone);
                app.handle_ice(ice_candidate.0, &ice_candidate.1)
                    .expect("couldn't handle sdp");
            });
    }
}

pub fn test(parent: SupervisorRef, type_: WebRTCBinActorType) {
    parent.supervisor(|s| {
        s.children(move |c| {
            c.with_distributor(Distributor::named(type_.as_ref()))
                .with_exec(move |ctx| run(ctx, type_))
        })
    });
}

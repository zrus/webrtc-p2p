use std::sync::{Arc, Mutex, Weak};

use bastion::context::BastionContext;
use bastion::distributor::Distributor;
use bastion::message::MessageHandler;
use bastion::supervisor::SupervisorRef;
use gst::element_error;
use gst::prelude::*;

use serde_derive::{Deserialize, Serialize};

use anyhow::{anyhow, bail, Context};

use crate::upgrade_weak;
use crate::utils;
use crate::webrtcbin_actor::SDPType;
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

                if let Err(err) = app.on_ice_candidate(mlineindex, candidate) {
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

    // Handle WebSocket messages, both our own as well as WebSocket protocol messages
    fn handle_websocket_message(&self, msg: &str) -> Result<(), anyhow::Error> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {}", msg);
        }

        let json_msg: JsonMsg = serde_json::from_str(msg)?;

        match json_msg {
            JsonMsg::Sdp { type_, sdp } => self.handle_sdp(&type_, &sdp),
            JsonMsg::Ice {
                sdp_mline_index,
                candidate,
            } => self.handle_ice(sdp_mline_index, &candidate),
        }
    }

    // Handle GStreamer messages coming from the pipeline
    fn handle_pipeline_message(&self, message: &gst::Message) -> Result<(), anyhow::Error> {
        use gst::message::MessageView;

        match message.view() {
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
            _ => (),
        }

        Ok(())
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

        let mut sdp = offer.sdp().as_text().unwrap();
        println!("Sending SDP offer to server:\n{}", sdp);
        sdp = utils::serialize(&SDPType::Offer, &sdp)?;

        // TODO
        Distributor::named("server")
            .tell_one(sdp)
            .expect("couldn't send to server");

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

        let mut sdp = answer.sdp().as_text().unwrap();
        println!("Sending SDP answer to client:\n{}", sdp);
        sdp = utils::serialize(&SDPType::Answer, &sdp)?;

        // TODO
        Distributor::named("client")
            .tell_one(sdp)
            .expect("couldn't send to client");

        Ok(())
    }

    // Handle incoming SDP answers from the peer
    fn handle_sdp(&self, type_: &str, sdp: &str) -> Result<(), anyhow::Error> {
        if type_ == "answer" {
            print!("Received answer:\n{}\n", sdp);

            let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .map_err(|_| anyhow!("Failed to parse SDP answer"))?;
            let answer =
                gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, ret);

            self.webrtcbin
                .emit_by_name("set-remote-description", &[&answer, &None::<gst::Promise>])
                .unwrap();

            Ok(())
        } else if type_ == "offer" {
            print!("Received offer:\n{}\n", sdp);

            let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                .map_err(|_| anyhow!("Failed to parse SDP offer"))?;

            // And then asynchronously start our pipeline and do the next steps. The
            // pipeline needs to be started before we can create an answer
            let app_clone = self.downgrade();
            self.pipeline.call_async(move |_pipeline| {
                let app = upgrade_weak!(app_clone);

                let offer = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Offer,
                    ret,
                );

                app.0
                    .webrtcbin
                    .emit_by_name("set-remote-description", &[&offer, &None::<gst::Promise>])
                    .unwrap();

                let app_clone = app.downgrade();
                let promise = gst::Promise::with_change_func(move |reply| {
                    let app = upgrade_weak!(app_clone);

                    if let Err(err) = app.on_answer_created(reply) {
                        element_error!(
                            app.pipeline,
                            gst::LibraryError::Failed,
                            ("Failed to send SDP answer: {:?}", err)
                        );
                    }
                });

                app.0
                    .webrtcbin
                    .emit_by_name("create-answer", &[&None::<gst::Structure>, &promise])
                    .unwrap();
            });

            Ok(())
        } else {
            bail!("Sdp type is not \"answer\" but \"{}\"", type_)
        }
    }

    // Handle incoming ICE candidates from the peer by passing them to webrtcbin
    fn handle_ice(&self, sdp_mline_index: u32, candidate: &str) -> Result<(), anyhow::Error> {
        self.webrtcbin
            .emit_by_name("add-ice-candidate", &[&sdp_mline_index, &candidate])
            .unwrap();

        Ok(())
    }

    // Asynchronously send ICE candidates to the peer via the WebSocket connection as a JSON
    // message
    fn on_ice_candidate(&self, mlineindex: u32, candidate: String) -> Result<(), anyhow::Error> {
        let message = serde_json::to_string(&JsonMsg::Ice {
            candidate,
            sdp_mline_index: mlineindex,
        })
        .unwrap();

        // TODO

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
        } else if name.starts_with("audio/") {
            gst::parse_bin_from_description(
                "queue ! audioconvert ! audioresample ! autoaudiosink",
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

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::message::MessageView;
        println!(".");
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
        MessageHandler::new(ctx.recv().await?).on_tell(|sdp: String, _| {
            println!("{}", sdp);
            bastion::run! { async {
                match utils::deserialize(&sdp).await {
                    Ok((type_, sdp)) => {
                        let type_ = match type_ {
                            gst_webrtc::WebRTCSDPType::Offer => "offer",
                            // gst_webrtc::WebRTCSDPType::Pranswer => todo!(),
                            gst_webrtc::WebRTCSDPType::Answer => "answer",
                            // gst_webrtc::WebRTCSDPType::Rollback => todo!(),
                            _ => return,
                        };
                        println!("sdp: {}", sdp);
                        let app = upgrade_weak!(app_clone);
                        app
                            .handle_sdp(type_, &sdp);
                    }
                    Err(e) => println!("{}", e),
                }
            }}
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

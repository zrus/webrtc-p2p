use std::sync::{Arc, Weak};

use anyhow::bail;
use bastion::{
    blocking,
    context::BastionContext,
    distributor::Distributor,
    message::MessageHandler,
    run,
    supervisor::{ActorRestartStrategy, RestartPolicy, RestartStrategy, SupervisorRef},
};
use gst::{
    glib,
    prelude::{Cast, ElementExtManual, ObjectExt, ToValue},
    traits::{ElementExt, GstBinExt, GstObjectExt},
};
use serde_json::{json, Value};

use crate::upgrade_weak;

type SDPType = gst_webrtc::WebRTCSDPType;
type SessionDescription = gst_webrtc::WebRTCSessionDescription;

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

// #[derive(Debug, Clone)]
// pub struct SDPMessage(SessionDescription);

// impl TryFrom<&str> for SDPMessage {
//     type Error = anyhow::Error;

//     fn try_from(value: &str) -> Result<Self, Self::Error> {
//         let data = base64::decode(value)?;
//         let json: Value = serde_json::from_slice(&data)?;
//         Self()
//     }
// }

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
    pub fn init(type_: &WebRTCBinActorType) -> Result<Self, anyhow::Error> {
        let pipeline = match type_ {
            &WebRTCBinActorType::Server => gst::parse_launch(
                "webrtcbin name=webrtcbin stun-server=stun://stun.l.google.com:19302 
                videotestsrc pattern=ball is-live=true ! video/x-raw,width=640,height=480,format=I420 ! 
                vp8enc error-resilient=partitions keyframe-max-dist=10 auto-alt-ref=true cpu-used=5 deadline=1 ! 
                rtpvp8pay ! webrtcbin.",
            )
            .expect("couldn't parse pipeline from string"),
            &WebRTCBinActorType::Client => gst::parse_launch(
                    "webrtcbin name=webrtcbin stun-server=stun://stun.l.google.com:19302 
                videotestsrc pattern=ball is-live=true ! video/x-raw,width=640,height=480,format=I420 ! 
                vp8enc error-resilient=partitions keyframe-max-dist=10 auto-alt-ref=true cpu-used=5 deadline=1 ! 
                rtpvp8pay ! webrtcbin.",
            )
            .expect("couldn't parse pipeline from string"),
        };

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");

        if let Some(transceiver) = webrtcbin
            .emit_by_name("get-transceiver", &[&0.to_value()])
            .unwrap()
            .and_then(|val| val.get::<glib::Object>().ok())
        {
            transceiver.set_property("do-nack", &false.to_value())?;
        }

        let pipeline = Self(Arc::new(WebRTCPipelineInner {
            pipeline,
            webrtcbin,
        }));

        let pl_clone = pipeline.downgrade();
        pipeline
            .webrtcbin
            .connect("on-negotiation-needed", false, move |_| {
                let pipeline = upgrade_weak!(pl_clone, None);
                if let Err(err) = pipeline.on_negotiation_needed() {
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
            .connect("on-ice-candidate", false, move |values| {
                let mlineindex = values[1].get::<u32>().expect("invalid argument");
                let candidate = values[2].get::<String>().expect("invalid argument");

                let pipeline = upgrade_weak!(pl_clone, None);

                if let Err(err) = pipeline.on_ice_candidate(mlineindex, candidate) {
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

    async fn handle_sdp(&self, type_: &SDPType, sdp: &str) -> Result<(), anyhow::Error> {
        match type_ {
            &SDPType::Answer => {
                print!("Received answer:\n{}\n", sdp);

                let mut json_answer = serde_json::to_string(sdp)
                    .expect("couldn't serialize local description to string");
                json_answer = json!({
                    "type": "answer",
                    "sdp": json_answer
                })
                .to_string();
                let b64 = base64::encode(&json_answer);
                println!("{}", b64);

                let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Failed to parse SDP answer"))?;

                let answer = SessionDescription::new(SDPType::Answer, ret);

                self.webrtcbin
                    .emit_by_name("set-remote-description", &[&answer, &None::<gst::Promise>])
                    .expect("couldn't set remote description for webrtcbin");

                Ok(())
            }
            &SDPType::Offer => {
                // println!("Received offer: \n{}\n", sdp);

                let b = base64::decode(sdp)?;
                let offer_json: Value = serde_json::from_slice(&b).expect("couldn't deserialize");
                let ret = gst_sdp::SDPMessage::parse_buffer(
                    &offer_json["sdp"].as_str().unwrap().as_bytes(),
                )?;

                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                let pl_clone = self.downgrade();
                self.pipeline.call_async(move |_| {
                    let pipeline = upgrade_weak!(pl_clone);
                    let offer = SessionDescription::new(SDPType::Offer, ret);
                    pipeline
                        .0
                        .webrtcbin
                        .emit_by_name("set-remote-description", &[&offer, &None::<gst::Promise>])
                        .expect("couldn't set remote description for webrtcbin");

                    let pl_clone = pipeline.downgrade();
                    let promise = gst::Promise::with_change_func(move |reply| {
                        let pipeline = upgrade_weak!(pl_clone);

                        run! { async {
                            if let Err(err) = pipeline.on_answer_created(reply).await {
                                gst::element_error!(
                                    pipeline.pipeline,
                                    gst::LibraryError::Failed,
                                    ("Failed to send SDP answer: {:?}", err)
                                );
                            }
                        }}
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

    fn on_ice_candidate(&self, mlineindex: u32, candidate: String) -> Result<(), anyhow::Error> {
        self.webrtcbin
            .emit_by_name("add-ice-candidate", &[&mlineindex, &candidate])
            .expect("couldn't add ice candidate");
        Ok(())
    }

    fn on_negotiation_needed(&self) -> Result<(), anyhow::Error> {
        println!("starting negotiation");

        let pl_clone = self.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let pipeline = upgrade_weak!(pl_clone);

            run! { async {
                if let Err(err) = pipeline.on_offer_created(reply).await {
                    gst::element_error!(
                        pipeline.pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to send SDP offer: {:?}", err)
                    );
                }
            }}
        });

        self.webrtcbin
            .emit_by_name("create-offer", &[&None::<gst::Structure>, &promise])
            .expect("couldn't create offer");

        Ok(())
    }

    async fn on_offer_created(
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
            .get::<SessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&offer, &None::<gst::Promise>])
            .expect("couldn't set local description");

        let sdp = offer.sdp().as_text().unwrap();

        println!(
            "sending SDP offer to peer: {}",
            offer.sdp().as_text().unwrap()
        );

        self.handle_sdp(&SDPType::Offer, &sdp).await?;

        Ok(())
    }

    async fn on_answer_created(
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
            .get::<SessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&answer, &None::<gst::Promise>])
            .expect("couldn't set local description for webrtcbin");

        let sdp = answer.sdp().as_text().unwrap();

        println!("sending SDP answer to peer: {}", sdp);

        self.handle_sdp(&SDPType::Answer, &sdp).await?;

        Ok(())
    }
}

fn main_loop(pipeline: WebRTCPipeline) -> Result<(), anyhow::Error> {
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
    println!("break");
    Ok(())
}

pub struct WebRTCBinActor;

impl WebRTCBinActor {
    pub fn run(parent: SupervisorRef, type_: WebRTCBinActorType) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default()
                        .with_restart_policy(RestartPolicy::Tries(5))
                        .with_actor_restart_strategy(ActorRestartStrategy::Immediate),
                )
                .children(move |c| {
                    c.with_distributor(Distributor::named(type_.as_ref()))
                        .with_exec(move |ctx| main_fn(ctx, type_))
                })
            })
            .expect("couldn't run Gstreamer actor");
    }
}

async fn main_fn(ctx: BastionContext, type_: WebRTCBinActorType) -> Result<(), ()> {
    println!("WebRTCBin started");
    gst::init().expect("couldn't initialize gstreamer");
    let pipeline = WebRTCPipeline::init(&type_).expect("couldn't create webrtcbin pipeline");
    pipeline.run().expect("couldn't start webrtc pipeline up");
    let pl_clone = pipeline.downgrade();
    blocking! {main_loop(pipeline)};
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|sdp: String, _| {
                run! { async {
                    let pipeline = upgrade_weak!(pl_clone);
                    pipeline
                        .handle_sdp(&SDPType::Offer, "")
                        .await
                        .expect("couldn't handle sdp");
                }}
            })
            .on_tell(|str: String, _| {
                run! { async {
                    let pipeline = upgrade_weak!(pl_clone);
                    pipeline
                        .handle_sdp(&SDPType::Answer, "")
                        .await
                        .expect("couldn't handle sdp");
                }}
            });
    }
}

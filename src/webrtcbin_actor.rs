use std::{
    str::FromStr,
    sync::{Arc, Weak},
};

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
    prelude::{Cast, ElementExtManual, GObjectExtManualGst, ObjectExt, ToValue},
    traits::{ElementExt, GstBinExt, GstObjectExt},
};
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
    fn create_client() -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch(
            "webrtcbin name=webrtcbin stun-server=stun://stun.l.google.com:19302 !
                rtpvp8depay ! vp8dec ! videoconvert ! fakevideosink",
        )
        .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        let direction = gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly;
        let caps =
            gst::Caps::from_str("application/x-rtp,media=video,encoding-name=VP8/9000,payload=96")?;
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

    fn create_server() -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch(
                "videotestsrc pattern=ball is-live=true ! video/x-raw,width=640,height=480,format=I420 ! 
                vp8enc error-resilient=partitions keyframe-max-dist=10 auto-alt-ref=true cpu-used=5 deadline=1 ! 
                rtpvp8pay pt=96 ! webrtcbin. webrtcbin name=webrtcbin",
            )
            .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let webrtcbin = pipeline.by_name("webrtcbin").expect("can't find webrtcbin");
        webrtcbin.set_property_from_str("stun-server", "stun://stun.l.google.com:19302");
        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

        let direction = gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly;
        let caps =
            gst::Caps::from_str("application/x-rtp,media=video,encoding-name=VP8,payload=96")?;
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

    pub fn init(type_: &WebRTCBinActorType) -> Result<Self, anyhow::Error> {
        match type_ {
            &WebRTCBinActorType::Server => Self::create_server(),
            &WebRTCBinActorType::Client => Self::create_client(),
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

    async fn handle_sdp(&self, type_: SDPType, sdp: String) -> Result<(), anyhow::Error> {
        match type_ {
            SDPType::Answer => {
                println!("Received answer:\n{}\n", sdp);

                let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Failed to parse SDP answer"))?;

                let answer = SessionDescription::new(SDPType::Answer, ret);

                self.webrtcbin
                    .emit_by_name("set-remote-description", &[&answer, &None::<gst::Promise>])
                    .expect("couldn't set remote description for webrtcbin");

                Ok(())
            }
            SDPType::Offer => {
                println!("Received offer: \n{}\n", sdp);

                let pl_clone = self.downgrade();
                self.pipeline.call_async(move |_| {
                    let pipeline = upgrade_weak!(pl_clone);
                    let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).unwrap();
                    let offer = SessionDescription::new(type_, sdp);
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
        println!("Starting negotiation");

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
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");
        self.webrtcbin
            .emit_by_name("set-local-description", &[&offer, &None::<gst::Promise>])
            .unwrap();

        let mut sdp = offer.sdp().as_text().unwrap();
        println!("Sending SDP offer to server:\n{}", sdp);
        sdp = utils::serialize(&SDPType::Offer, &sdp)?;

        // let sdp = "eyJ0eXBlIjoib2ZmZXIiLCJzZHAiOiJ2PTBcclxubz0tIDE3OTgzODA1OTE0MTgxNTEyMTMgMiBJTiBJUDQgMTI3LjAuMC4xXHJcbnM9LVxyXG50PTAgMFxyXG5hPWdyb3VwOkJVTkRMRSAwIDEgMlxyXG5hPWV4dG1hcC1hbGxvdy1taXhlZFxyXG5hPW1zaWQtc2VtYW50aWM6IFdNU1xyXG5tPWF1ZGlvIDYzMjUwIFVEUC9UTFMvUlRQL1NBVlBGIDExMSA2MyAxMDMgMTA0IDkgMCA4IDEwNiAxMDUgMTMgMTEwIDExMiAxMTMgMTI2XHJcbmM9SU4gSVA0IDExMy4xNjEuOTUuNTNcclxuYT1ydGNwOjkgSU4gSVA0IDAuMC4wLjBcclxuYT1jYW5kaWRhdGU6Njk2MTIyNDk3IDEgdWRwIDIxMTM5MzcxNTEgZjM2ZjA5ODItNDkwYy00MzU1LWE3YmYtZTU1OTg5MzEzNTUzLmxvY2FsIDU5Nzk0IHR5cCBob3N0IGdlbmVyYXRpb24gMCBuZXR3b3JrLWNvc3QgOTk5XHJcbmE9Y2FuZGlkYXRlOjg0MjE2MzA0OSAxIHVkcCAxNjc3NzI5NTM1IDExMy4xNjEuOTUuNTMgNjMyNTAgdHlwIHNyZmx4IHJhZGRyIDAuMC4wLjAgcnBvcnQgMCBnZW5lcmF0aW9uIDAgbmV0d29yay1jb3N0IDk5OVxyXG5hPWljZS11ZnJhZzpTcjZkXHJcbmE9aWNlLXB3ZDpDTTgrVHV3VllEL241c212ZnBwNjNHUURcclxuYT1pY2Utb3B0aW9uczp0cmlja2xlXHJcbmE9ZmluZ2VycHJpbnQ6c2hhLTI1NiAxMTo5OToxMTpDNTo4RTpDRDoyNTpCOTo2QzozQjo5ODpFMDo0MTpCNDowNzpBQjoyRTpERjoxQzo1MzpCNTowRDo0OTpCNTo4NzpDOTpFMjo0MzpBMjo1MjpFMzo4OVxyXG5hPXNldHVwOmFjdHBhc3NcclxuYT1taWQ6MFxyXG5hPWV4dG1hcDoxIHVybjppZXRmOnBhcmFtczpydHAtaGRyZXh0OnNzcmMtYXVkaW8tbGV2ZWxcclxuYT1leHRtYXA6MiBodHRwOi8vd3d3LndlYnJ0Yy5vcmcvZXhwZXJpbWVudHMvcnRwLWhkcmV4dC9hYnMtc2VuZC10aW1lXHJcbmE9ZXh0bWFwOjMgaHR0cDovL3d3dy5pZXRmLm9yZy9pZC9kcmFmdC1ob2xtZXItcm1jYXQtdHJhbnNwb3J0LXdpZGUtY2MtZXh0ZW5zaW9ucy0wMVxyXG5hPWV4dG1hcDo0IHVybjppZXRmOnBhcmFtczpydHAtaGRyZXh0OnNkZXM6bWlkXHJcbmE9cmVjdm9ubHlcclxuYT1ydGNwLW11eFxyXG5hPXJ0cG1hcDoxMTEgb3B1cy80ODAwMC8yXHJcbmE9cnRjcC1mYjoxMTEgdHJhbnNwb3J0LWNjXHJcbmE9Zm10cDoxMTEgbWlucHRpbWU9MTA7dXNlaW5iYW5kZmVjPTFcclxuYT1ydHBtYXA6NjMgcmVkLzQ4MDAwLzJcclxuYT1mbXRwOjYzIDExMS8xMTFcclxuYT1ydHBtYXA6MTAzIElTQUMvMTYwMDBcclxuYT1ydHBtYXA6MTA0IElTQUMvMzIwMDBcclxuYT1ydHBtYXA6OSBHNzIyLzgwMDBcclxuYT1ydHBtYXA6MCBQQ01VLzgwMDBcclxuYT1ydHBtYXA6OCBQQ01BLzgwMDBcclxuYT1ydHBtYXA6MTA2IENOLzMyMDAwXHJcbmE9cnRwbWFwOjEwNSBDTi8xNjAwMFxyXG5hPXJ0cG1hcDoxMyBDTi84MDAwXHJcbmE9cnRwbWFwOjExMCB0ZWxlcGhvbmUtZXZlbnQvNDgwMDBcclxuYT1ydHBtYXA6MTEyIHRlbGVwaG9uZS1ldmVudC8zMjAwMFxyXG5hPXJ0cG1hcDoxMTMgdGVsZXBob25lLWV2ZW50LzE2MDAwXHJcbmE9cnRwbWFwOjEyNiB0ZWxlcGhvbmUtZXZlbnQvODAwMFxyXG5tPXZpZGVvIDYzMjUyIFVEUC9UTFMvUlRQL1NBVlBGIDk2IDk3IDk4IDk5IDEwMCAxMDEgMTAyIDEyMiAxMjcgMTIxIDEyNSAxMDcgMTA4IDEwOSAxMjQgMTIwIDEyMyAxMTkgMzUgMzYgMzcgMzggMzkgNDAgNDEgNDIgMTE0IDExNSAxMTYgMTE3IDExOCA0M1xyXG5jPUlOIElQNCAxMTMuMTYxLjk1LjUzXHJcbmE9cnRjcDo5IElOIElQNCAwLjAuMC4wXHJcbmE9Y2FuZGlkYXRlOjY5NjEyMjQ5NyAxIHVkcCAyMTEzOTM3MTUxIGYzNmYwOTgyLTQ5MGMtNDM1NS1hN2JmLWU1NTk4OTMxMzU1My5sb2NhbCA1OTc5NiB0eXAgaG9zdCBnZW5lcmF0aW9uIDAgbmV0d29yay1jb3N0IDk5OVxyXG5hPWNhbmRpZGF0ZTo4NDIxNjMwNDkgMSB1ZHAgMTY3NzcyOTUzNSAxMTMuMTYxLjk1LjUzIDYzMjUyIHR5cCBzcmZseCByYWRkciAwLjAuMC4wIHJwb3J0IDAgZ2VuZXJhdGlvbiAwIG5ldHdvcmstY29zdCA5OTlcclxuYT1pY2UtdWZyYWc6U3I2ZFxyXG5hPWljZS1wd2Q6Q004K1R1d1ZZRC9uNXNtdmZwcDYzR1FEXHJcbmE9aWNlLW9wdGlvbnM6dHJpY2tsZVxyXG5hPWZpbmdlcnByaW50OnNoYS0yNTYgMTE6OTk6MTE6QzU6OEU6Q0Q6MjU6Qjk6NkM6M0I6OTg6RTA6NDE6QjQ6MDc6QUI6MkU6REY6MUM6NTM6QjU6MEQ6NDk6QjU6ODc6Qzk6RTI6NDM6QTI6NTI6RTM6ODlcclxuYT1zZXR1cDphY3RwYXNzXHJcbmE9bWlkOjFcclxuYT1leHRtYXA6MTQgdXJuOmlldGY6cGFyYW1zOnJ0cC1oZHJleHQ6dG9mZnNldFxyXG5hPWV4dG1hcDoyIGh0dHA6Ly93d3cud2VicnRjLm9yZy9leHBlcmltZW50cy9ydHAtaGRyZXh0L2Ficy1zZW5kLXRpbWVcclxuYT1leHRtYXA6MTMgdXJuOjNncHA6dmlkZW8tb3JpZW50YXRpb25cclxuYT1leHRtYXA6MyBodHRwOi8vd3d3LmlldGYub3JnL2lkL2RyYWZ0LWhvbG1lci1ybWNhdC10cmFuc3BvcnQtd2lkZS1jYy1leHRlbnNpb25zLTAxXHJcbmE9ZXh0bWFwOjUgaHR0cDovL3d3dy53ZWJydGMub3JnL2V4cGVyaW1lbnRzL3J0cC1oZHJleHQvcGxheW91dC1kZWxheVxyXG5hPWV4dG1hcDo2IGh0dHA6Ly93d3cud2VicnRjLm9yZy9leHBlcmltZW50cy9ydHAtaGRyZXh0L3ZpZGVvLWNvbnRlbnQtdHlwZVxyXG5hPWV4dG1hcDo3IGh0dHA6Ly93d3cud2VicnRjLm9yZy9leHBlcmltZW50cy9ydHAtaGRyZXh0L3ZpZGVvLXRpbWluZ1xyXG5hPWV4dG1hcDo4IGh0dHA6Ly93d3cud2VicnRjLm9yZy9leHBlcmltZW50cy9ydHAtaGRyZXh0L2NvbG9yLXNwYWNlXHJcbmE9ZXh0bWFwOjQgdXJuOmlldGY6cGFyYW1zOnJ0cC1oZHJleHQ6c2RlczptaWRcclxuYT1leHRtYXA6MTAgdXJuOmlldGY6cGFyYW1zOnJ0cC1oZHJleHQ6c2RlczpydHAtc3RyZWFtLWlkXHJcbmE9ZXh0bWFwOjExIHVybjppZXRmOnBhcmFtczpydHAtaGRyZXh0OnNkZXM6cmVwYWlyZWQtcnRwLXN0cmVhbS1pZFxyXG5hPXJlY3Zvbmx5XHJcbmE9cnRjcC1tdXhcclxuYT1ydGNwLXJzaXplXHJcbmE9cnRwbWFwOjk2IFZQOC85MDAwMFxyXG5hPXJ0Y3AtZmI6OTYgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjo5NiB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjk2IGNjbSBmaXJcclxuYT1ydGNwLWZiOjk2IG5hY2tcclxuYT1ydGNwLWZiOjk2IG5hY2sgcGxpXHJcbmE9cnRwbWFwOjk3IHJ0eC85MDAwMFxyXG5hPWZtdHA6OTcgYXB0PTk2XHJcbmE9cnRwbWFwOjk4IFZQOS85MDAwMFxyXG5hPXJ0Y3AtZmI6OTggZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjo5OCB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjk4IGNjbSBmaXJcclxuYT1ydGNwLWZiOjk4IG5hY2tcclxuYT1ydGNwLWZiOjk4IG5hY2sgcGxpXHJcbmE9Zm10cDo5OCBwcm9maWxlLWlkPTBcclxuYT1ydHBtYXA6OTkgcnR4LzkwMDAwXHJcbmE9Zm10cDo5OSBhcHQ9OThcclxuYT1ydHBtYXA6MTAwIFZQOS85MDAwMFxyXG5hPXJ0Y3AtZmI6MTAwIGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTAwIHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTAwIGNjbSBmaXJcclxuYT1ydGNwLWZiOjEwMCBuYWNrXHJcbmE9cnRjcC1mYjoxMDAgbmFjayBwbGlcclxuYT1mbXRwOjEwMCBwcm9maWxlLWlkPTJcclxuYT1ydHBtYXA6MTAxIHJ0eC85MDAwMFxyXG5hPWZtdHA6MTAxIGFwdD0xMDBcclxuYT1ydHBtYXA6MTAyIFZQOS85MDAwMFxyXG5hPXJ0Y3AtZmI6MTAyIGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTAyIHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTAyIGNjbSBmaXJcclxuYT1ydGNwLWZiOjEwMiBuYWNrXHJcbmE9cnRjcC1mYjoxMDIgbmFjayBwbGlcclxuYT1mbXRwOjEwMiBwcm9maWxlLWlkPTFcclxuYT1ydHBtYXA6MTIyIHJ0eC85MDAwMFxyXG5hPWZtdHA6MTIyIGFwdD0xMDJcclxuYT1ydHBtYXA6MTI3IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjEyNyBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjEyNyB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjEyNyBjY20gZmlyXHJcbmE9cnRjcC1mYjoxMjcgbmFja1xyXG5hPXJ0Y3AtZmI6MTI3IG5hY2sgcGxpXHJcbmE9Zm10cDoxMjcgbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MTtwcm9maWxlLWxldmVsLWlkPTQyMDAxZlxyXG5hPXJ0cG1hcDoxMjEgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMjEgYXB0PTEyN1xyXG5hPXJ0cG1hcDoxMjUgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MTI1IGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTI1IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTI1IGNjbSBmaXJcclxuYT1ydGNwLWZiOjEyNSBuYWNrXHJcbmE9cnRjcC1mYjoxMjUgbmFjayBwbGlcclxuYT1mbXRwOjEyNSBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0wO3Byb2ZpbGUtbGV2ZWwtaWQ9NDIwMDFmXHJcbmE9cnRwbWFwOjEwNyBydHgvOTAwMDBcclxuYT1mbXRwOjEwNyBhcHQ9MTI1XHJcbmE9cnRwbWFwOjEwOCBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjoxMDggZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMDggdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMDggY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTA4IG5hY2tcclxuYT1ydGNwLWZiOjEwOCBuYWNrIHBsaVxyXG5hPWZtdHA6MTA4IGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTE7cHJvZmlsZS1sZXZlbC1pZD00MmUwMWZcclxuYT1ydHBtYXA6MTA5IHJ0eC85MDAwMFxyXG5hPWZtdHA6MTA5IGFwdD0xMDhcclxuYT1ydHBtYXA6MTI0IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjEyNCBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjEyNCB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjEyNCBjY20gZmlyXHJcbmE9cnRjcC1mYjoxMjQgbmFja1xyXG5hPXJ0Y3AtZmI6MTI0IG5hY2sgcGxpXHJcbmE9Zm10cDoxMjQgbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MDtwcm9maWxlLWxldmVsLWlkPTQyZTAxZlxyXG5hPXJ0cG1hcDoxMjAgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMjAgYXB0PTEyNFxyXG5hPXJ0cG1hcDoxMjMgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MTIzIGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTIzIHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTIzIGNjbSBmaXJcclxuYT1ydGNwLWZiOjEyMyBuYWNrXHJcbmE9cnRjcC1mYjoxMjMgbmFjayBwbGlcclxuYT1mbXRwOjEyMyBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0xO3Byb2ZpbGUtbGV2ZWwtaWQ9NGQwMDFmXHJcbmE9cnRwbWFwOjExOSBydHgvOTAwMDBcclxuYT1mbXRwOjExOSBhcHQ9MTIzXHJcbmE9cnRwbWFwOjM1IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjM1IGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MzUgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjozNSBjY20gZmlyXHJcbmE9cnRjcC1mYjozNSBuYWNrXHJcbmE9cnRjcC1mYjozNSBuYWNrIHBsaVxyXG5hPWZtdHA6MzUgbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MDtwcm9maWxlLWxldmVsLWlkPTRkMDAxZlxyXG5hPXJ0cG1hcDozNiBydHgvOTAwMDBcclxuYT1mbXRwOjM2IGFwdD0zNVxyXG5hPXJ0cG1hcDozNyBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjozNyBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjM3IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MzcgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MzcgbmFja1xyXG5hPXJ0Y3AtZmI6MzcgbmFjayBwbGlcclxuYT1mbXRwOjM3IGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTE7cHJvZmlsZS1sZXZlbC1pZD1mNDAwMWZcclxuYT1ydHBtYXA6MzggcnR4LzkwMDAwXHJcbmE9Zm10cDozOCBhcHQ9MzdcclxuYT1ydHBtYXA6MzkgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MzkgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjozOSB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjM5IGNjbSBmaXJcclxuYT1ydGNwLWZiOjM5IG5hY2tcclxuYT1ydGNwLWZiOjM5IG5hY2sgcGxpXHJcbmE9Zm10cDozOSBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0wO3Byb2ZpbGUtbGV2ZWwtaWQ9ZjQwMDFmXHJcbmE9cnRwbWFwOjQwIHJ0eC85MDAwMFxyXG5hPWZtdHA6NDAgYXB0PTM5XHJcbmE9cnRwbWFwOjQxIEFWMS85MDAwMFxyXG5hPXJ0Y3AtZmI6NDEgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjo0MSB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjQxIGNjbSBmaXJcclxuYT1ydGNwLWZiOjQxIG5hY2tcclxuYT1ydGNwLWZiOjQxIG5hY2sgcGxpXHJcbmE9cnRwbWFwOjQyIHJ0eC85MDAwMFxyXG5hPWZtdHA6NDIgYXB0PTQxXHJcbmE9cnRwbWFwOjExNCBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjoxMTQgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMTQgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMTQgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTE0IG5hY2tcclxuYT1ydGNwLWZiOjExNCBuYWNrIHBsaVxyXG5hPWZtdHA6MTE0IGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTE7cHJvZmlsZS1sZXZlbC1pZD02NDAwMWZcclxuYT1ydHBtYXA6MTE1IHJ0eC85MDAwMFxyXG5hPWZtdHA6MTE1IGFwdD0xMTRcclxuYT1ydHBtYXA6MTE2IHJlZC85MDAwMFxyXG5hPXJ0cG1hcDoxMTcgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMTcgYXB0PTExNlxyXG5hPXJ0cG1hcDoxMTggdWxwZmVjLzkwMDAwXHJcbmE9cnRwbWFwOjQzIGZsZXhmZWMtMDMvOTAwMDBcclxuYT1ydGNwLWZiOjQzIGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6NDMgdHJhbnNwb3J0LWNjXHJcbmE9Zm10cDo0MyByZXBhaXItd2luZG93PTEwMDAwMDAwXHJcbm09dmlkZW8gNjMyNTQgVURQL1RMUy9SVFAvU0FWUEYgOTYgOTcgOTggOTkgMTAwIDEwMSAxMDIgMTIyIDEyNyAxMjEgMTI1IDEwNyAxMDggMTA5IDEyNCAxMjAgMTIzIDExOSAzNSAzNiAzNyAzOCAzOSA0MCA0MSA0MiAxMTQgMTE1IDExNiAxMTcgMTE4IDQzXHJcbmM9SU4gSVA0IDExMy4xNjEuOTUuNTNcclxuYT1ydGNwOjkgSU4gSVA0IDAuMC4wLjBcclxuYT1jYW5kaWRhdGU6Njk2MTIyNDk3IDEgdWRwIDIxMTM5MzcxNTEgZjM2ZjA5ODItNDkwYy00MzU1LWE3YmYtZTU1OTg5MzEzNTUzLmxvY2FsIDU5Nzk4IHR5cCBob3N0IGdlbmVyYXRpb24gMCBuZXR3b3JrLWNvc3QgOTk5XHJcbmE9Y2FuZGlkYXRlOjg0MjE2MzA0OSAxIHVkcCAxNjc3NzI5NTM1IDExMy4xNjEuOTUuNTMgNjMyNTQgdHlwIHNyZmx4IHJhZGRyIDAuMC4wLjAgcnBvcnQgMCBnZW5lcmF0aW9uIDAgbmV0d29yay1jb3N0IDk5OVxyXG5hPWljZS11ZnJhZzpTcjZkXHJcbmE9aWNlLXB3ZDpDTTgrVHV3VllEL241c212ZnBwNjNHUURcclxuYT1pY2Utb3B0aW9uczp0cmlja2xlXHJcbmE9ZmluZ2VycHJpbnQ6c2hhLTI1NiAxMTo5OToxMTpDNTo4RTpDRDoyNTpCOTo2QzozQjo5ODpFMDo0MTpCNDowNzpBQjoyRTpERjoxQzo1MzpCNTowRDo0OTpCNTo4NzpDOTpFMjo0MzpBMjo1MjpFMzo4OVxyXG5hPXNldHVwOmFjdHBhc3NcclxuYT1taWQ6MlxyXG5hPWV4dG1hcDoxNCB1cm46aWV0ZjpwYXJhbXM6cnRwLWhkcmV4dDp0b2Zmc2V0XHJcbmE9ZXh0bWFwOjIgaHR0cDovL3d3dy53ZWJydGMub3JnL2V4cGVyaW1lbnRzL3J0cC1oZHJleHQvYWJzLXNlbmQtdGltZVxyXG5hPWV4dG1hcDoxMyB1cm46M2dwcDp2aWRlby1vcmllbnRhdGlvblxyXG5hPWV4dG1hcDozIGh0dHA6Ly93d3cuaWV0Zi5vcmcvaWQvZHJhZnQtaG9sbWVyLXJtY2F0LXRyYW5zcG9ydC13aWRlLWNjLWV4dGVuc2lvbnMtMDFcclxuYT1leHRtYXA6NSBodHRwOi8vd3d3LndlYnJ0Yy5vcmcvZXhwZXJpbWVudHMvcnRwLWhkcmV4dC9wbGF5b3V0LWRlbGF5XHJcbmE9ZXh0bWFwOjYgaHR0cDovL3d3dy53ZWJydGMub3JnL2V4cGVyaW1lbnRzL3J0cC1oZHJleHQvdmlkZW8tY29udGVudC10eXBlXHJcbmE9ZXh0bWFwOjcgaHR0cDovL3d3dy53ZWJydGMub3JnL2V4cGVyaW1lbnRzL3J0cC1oZHJleHQvdmlkZW8tdGltaW5nXHJcbmE9ZXh0bWFwOjggaHR0cDovL3d3dy53ZWJydGMub3JnL2V4cGVyaW1lbnRzL3J0cC1oZHJleHQvY29sb3Itc3BhY2VcclxuYT1leHRtYXA6NCB1cm46aWV0ZjpwYXJhbXM6cnRwLWhkcmV4dDpzZGVzOm1pZFxyXG5hPWV4dG1hcDoxMCB1cm46aWV0ZjpwYXJhbXM6cnRwLWhkcmV4dDpzZGVzOnJ0cC1zdHJlYW0taWRcclxuYT1leHRtYXA6MTEgdXJuOmlldGY6cGFyYW1zOnJ0cC1oZHJleHQ6c2RlczpyZXBhaXJlZC1ydHAtc3RyZWFtLWlkXHJcbmE9cmVjdm9ubHlcclxuYT1ydGNwLW11eFxyXG5hPXJ0Y3AtcnNpemVcclxuYT1ydHBtYXA6OTYgVlA4LzkwMDAwXHJcbmE9cnRjcC1mYjo5NiBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjk2IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6OTYgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6OTYgbmFja1xyXG5hPXJ0Y3AtZmI6OTYgbmFjayBwbGlcclxuYT1ydHBtYXA6OTcgcnR4LzkwMDAwXHJcbmE9Zm10cDo5NyBhcHQ9OTZcclxuYT1ydHBtYXA6OTggVlA5LzkwMDAwXHJcbmE9cnRjcC1mYjo5OCBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjk4IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6OTggY2NtIGZpclxyXG5hPXJ0Y3AtZmI6OTggbmFja1xyXG5hPXJ0Y3AtZmI6OTggbmFjayBwbGlcclxuYT1mbXRwOjk4IHByb2ZpbGUtaWQ9MFxyXG5hPXJ0cG1hcDo5OSBydHgvOTAwMDBcclxuYT1mbXRwOjk5IGFwdD05OFxyXG5hPXJ0cG1hcDoxMDAgVlA5LzkwMDAwXHJcbmE9cnRjcC1mYjoxMDAgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMDAgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMDAgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTAwIG5hY2tcclxuYT1ydGNwLWZiOjEwMCBuYWNrIHBsaVxyXG5hPWZtdHA6MTAwIHByb2ZpbGUtaWQ9MlxyXG5hPXJ0cG1hcDoxMDEgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMDEgYXB0PTEwMFxyXG5hPXJ0cG1hcDoxMDIgVlA5LzkwMDAwXHJcbmE9cnRjcC1mYjoxMDIgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMDIgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMDIgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTAyIG5hY2tcclxuYT1ydGNwLWZiOjEwMiBuYWNrIHBsaVxyXG5hPWZtdHA6MTAyIHByb2ZpbGUtaWQ9MVxyXG5hPXJ0cG1hcDoxMjIgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMjIgYXB0PTEwMlxyXG5hPXJ0cG1hcDoxMjcgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MTI3IGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTI3IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTI3IGNjbSBmaXJcclxuYT1ydGNwLWZiOjEyNyBuYWNrXHJcbmE9cnRjcC1mYjoxMjcgbmFjayBwbGlcclxuYT1mbXRwOjEyNyBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0xO3Byb2ZpbGUtbGV2ZWwtaWQ9NDIwMDFmXHJcbmE9cnRwbWFwOjEyMSBydHgvOTAwMDBcclxuYT1mbXRwOjEyMSBhcHQ9MTI3XHJcbmE9cnRwbWFwOjEyNSBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjoxMjUgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMjUgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMjUgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTI1IG5hY2tcclxuYT1ydGNwLWZiOjEyNSBuYWNrIHBsaVxyXG5hPWZtdHA6MTI1IGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTA7cHJvZmlsZS1sZXZlbC1pZD00MjAwMWZcclxuYT1ydHBtYXA6MTA3IHJ0eC85MDAwMFxyXG5hPWZtdHA6MTA3IGFwdD0xMjVcclxuYT1ydHBtYXA6MTA4IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjEwOCBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjEwOCB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjEwOCBjY20gZmlyXHJcbmE9cnRjcC1mYjoxMDggbmFja1xyXG5hPXJ0Y3AtZmI6MTA4IG5hY2sgcGxpXHJcbmE9Zm10cDoxMDggbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MTtwcm9maWxlLWxldmVsLWlkPTQyZTAxZlxyXG5hPXJ0cG1hcDoxMDkgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMDkgYXB0PTEwOFxyXG5hPXJ0cG1hcDoxMjQgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MTI0IGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MTI0IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MTI0IGNjbSBmaXJcclxuYT1ydGNwLWZiOjEyNCBuYWNrXHJcbmE9cnRjcC1mYjoxMjQgbmFjayBwbGlcclxuYT1mbXRwOjEyNCBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0wO3Byb2ZpbGUtbGV2ZWwtaWQ9NDJlMDFmXHJcbmE9cnRwbWFwOjEyMCBydHgvOTAwMDBcclxuYT1mbXRwOjEyMCBhcHQ9MTI0XHJcbmE9cnRwbWFwOjEyMyBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjoxMjMgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjoxMjMgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjoxMjMgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MTIzIG5hY2tcclxuYT1ydGNwLWZiOjEyMyBuYWNrIHBsaVxyXG5hPWZtdHA6MTIzIGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTE7cHJvZmlsZS1sZXZlbC1pZD00ZDAwMWZcclxuYT1ydHBtYXA6MTE5IHJ0eC85MDAwMFxyXG5hPWZtdHA6MTE5IGFwdD0xMjNcclxuYT1ydHBtYXA6MzUgSDI2NC85MDAwMFxyXG5hPXJ0Y3AtZmI6MzUgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjozNSB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjM1IGNjbSBmaXJcclxuYT1ydGNwLWZiOjM1IG5hY2tcclxuYT1ydGNwLWZiOjM1IG5hY2sgcGxpXHJcbmE9Zm10cDozNSBsZXZlbC1hc3ltbWV0cnktYWxsb3dlZD0xO3BhY2tldGl6YXRpb24tbW9kZT0wO3Byb2ZpbGUtbGV2ZWwtaWQ9NGQwMDFmXHJcbmE9cnRwbWFwOjM2IHJ0eC85MDAwMFxyXG5hPWZtdHA6MzYgYXB0PTM1XHJcbmE9cnRwbWFwOjM3IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjM3IGdvb2ctcmVtYlxyXG5hPXJ0Y3AtZmI6MzcgdHJhbnNwb3J0LWNjXHJcbmE9cnRjcC1mYjozNyBjY20gZmlyXHJcbmE9cnRjcC1mYjozNyBuYWNrXHJcbmE9cnRjcC1mYjozNyBuYWNrIHBsaVxyXG5hPWZtdHA6MzcgbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MTtwcm9maWxlLWxldmVsLWlkPWY0MDAxZlxyXG5hPXJ0cG1hcDozOCBydHgvOTAwMDBcclxuYT1mbXRwOjM4IGFwdD0zN1xyXG5hPXJ0cG1hcDozOSBIMjY0LzkwMDAwXHJcbmE9cnRjcC1mYjozOSBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjM5IHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6MzkgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6MzkgbmFja1xyXG5hPXJ0Y3AtZmI6MzkgbmFjayBwbGlcclxuYT1mbXRwOjM5IGxldmVsLWFzeW1tZXRyeS1hbGxvd2VkPTE7cGFja2V0aXphdGlvbi1tb2RlPTA7cHJvZmlsZS1sZXZlbC1pZD1mNDAwMWZcclxuYT1ydHBtYXA6NDAgcnR4LzkwMDAwXHJcbmE9Zm10cDo0MCBhcHQ9MzlcclxuYT1ydHBtYXA6NDEgQVYxLzkwMDAwXHJcbmE9cnRjcC1mYjo0MSBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjQxIHRyYW5zcG9ydC1jY1xyXG5hPXJ0Y3AtZmI6NDEgY2NtIGZpclxyXG5hPXJ0Y3AtZmI6NDEgbmFja1xyXG5hPXJ0Y3AtZmI6NDEgbmFjayBwbGlcclxuYT1ydHBtYXA6NDIgcnR4LzkwMDAwXHJcbmE9Zm10cDo0MiBhcHQ9NDFcclxuYT1ydHBtYXA6MTE0IEgyNjQvOTAwMDBcclxuYT1ydGNwLWZiOjExNCBnb29nLXJlbWJcclxuYT1ydGNwLWZiOjExNCB0cmFuc3BvcnQtY2NcclxuYT1ydGNwLWZiOjExNCBjY20gZmlyXHJcbmE9cnRjcC1mYjoxMTQgbmFja1xyXG5hPXJ0Y3AtZmI6MTE0IG5hY2sgcGxpXHJcbmE9Zm10cDoxMTQgbGV2ZWwtYXN5bW1ldHJ5LWFsbG93ZWQ9MTtwYWNrZXRpemF0aW9uLW1vZGU9MTtwcm9maWxlLWxldmVsLWlkPTY0MDAxZlxyXG5hPXJ0cG1hcDoxMTUgcnR4LzkwMDAwXHJcbmE9Zm10cDoxMTUgYXB0PTExNFxyXG5hPXJ0cG1hcDoxMTYgcmVkLzkwMDAwXHJcbmE9cnRwbWFwOjExNyBydHgvOTAwMDBcclxuYT1mbXRwOjExNyBhcHQ9MTE2XHJcbmE9cnRwbWFwOjExOCB1bHBmZWMvOTAwMDBcclxuYT1ydHBtYXA6NDMgZmxleGZlYy0wMy85MDAwMFxyXG5hPXJ0Y3AtZmI6NDMgZ29vZy1yZW1iXHJcbmE9cnRjcC1mYjo0MyB0cmFuc3BvcnQtY2NcclxuYT1mbXRwOjQzIHJlcGFpci13aW5kb3c9MTAwMDAwMDBcclxuIn0=";

        Distributor::named("server")
            .tell_one(sdp)
            .expect("couldn't send SDP offer to server");

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

        let mut sdp = answer.sdp().as_text().unwrap();
        println!("Sending SDP answer to client:\n{}", sdp);
        sdp = utils::serialize(&SDPType::Answer, &sdp)?;

        Distributor::named("client")
            .tell_one("sdp")
            .expect("couldn't send SDP answer to client");

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
    println!("Break");
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
    println!("WebRTCBin {} started", type_.as_ref());
    gst::init().expect("couldn't initialize gstreamer");
    let pipeline = WebRTCPipeline::init(&type_).expect("couldn't create webrtcbin pipeline");
    pipeline.run().expect("couldn't start webrtc pipeline up");
    let pl_clone = pipeline.downgrade();
    blocking! {main_loop(pipeline)};
    loop {
        MessageHandler::new(ctx.recv().await?).on_tell(|sdp: String, _| {
            println!("WebRTCBin {} received message: {}", type_.as_ref(), sdp);
            run! { async {
                match utils::deserialize(&sdp).await {
                    Ok((type_, sdp)) => {
                        let pipeline = upgrade_weak!(pl_clone);
                        pipeline
                            .handle_sdp(type_, sdp)
                            .await
                            .expect("couldn't handle sdp");
                    }
                    Err(e) => println!("{}", e),
                }
            }}
        });
    }
}

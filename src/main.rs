#![allow(dead_code, unused, warnings)]

extern crate lazy_static;

mod gstreamer_actor;
mod nats_actor;
mod pipeline;
mod utils;
mod web_socket;
mod webrtc_actor;
mod webrtcbin_actor;

use anyhow::Result;
use bastion::prelude::*;
use nats_actor::NatsActor;
use web_socket::WsActor;
use webrtc_actor::WebRtcActor;
use webrtcbin_actor::{WebRTCBinActor, WebRTCBinActorType};

#[tokio::main]
async fn main() {
    Bastion::init();
    Bastion::start();

    let num_of_cam = 4;

    let nats = Bastion::supervisor(|s| s).unwrap();
    NatsActor::run(nats, num_of_cam);

    Bastion::block_until_stopped();
}

#[cfg(feature = "webrtcbin")]
fn main_fn() -> Result<(), anyhow::Error> {
    // MY WORKS

    let server_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(server_parent, WebRTCBinActorType::Server);

    let client_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(client_parent, WebRTCBinActorType::Client);

    // EXAMPLE FROM GSTREAMER

    // let server_parent = Bastion::supervisor(|s| s).unwrap();
    // sendrecv::test(server_parent, WebRTCBinActorType::Server);

    // let client_parent = Bastion::supervisor(|s| s).unwrap();
    // sendrecv::test(client_parent, WebRTCBinActorType::Client);

    Ok(())
}

// #[cfg(any(not(feature = "webrtcbin"), feature = "webrtc-rs"))]
// fn main_fn() -> Result<(), anyhow::Error> {
//     use webrtc_actor::WebRtcActor;

//     let mut line = String::new();

//     std::io::stdin().read_line(&mut line)?;
//     line = line.trim().to_owned();

//     let parent = Bastion::supervisor(|s| s).unwrap();
//     WebRtcActor::run(parent, &line);

//     Ok(())
// }

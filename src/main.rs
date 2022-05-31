#![allow(dead_code, unused, warnings)]

extern crate lazy_static;

mod gstreamer_actor;
mod pipeline;
mod sendrecv;
mod utils;
mod web_socket;
mod webrtc_actor;
mod webrtcbin_actor;

use anyhow::Result;
use bastion::prelude::*;
use web_socket::WsActor;
use webrtcbin_actor::{WebRTCBinActor, WebRTCBinActorType};

#[tokio::main]
async fn main() {
    Bastion::init();
    Bastion::start();

    for i in 1..=7 {
        if i == 3 {
            continue;
        }
        let server_parent = Bastion::supervisor(|s| s).unwrap();
        WebRTCBinActor::run(server_parent, WebRTCBinActorType::Server, i);

        let ws_server = Bastion::supervisor(|s| s).unwrap();
        WsActor::run(ws_server, i);
    }

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

#[cfg(any(not(feature = "webrtcbin"), feature = "webrtc-rs"))]
fn main_fn() -> Result<(), anyhow::Error> {
    use webrtc_actor::WebRtcActor;

    let mut line = String::new();

    std::io::stdin().read_line(&mut line)?;
    line = line.trim().to_owned();

    let parent = Bastion::supervisor(|s| s).unwrap();
    WebRtcActor::run(parent, &line);

    Ok(())
}

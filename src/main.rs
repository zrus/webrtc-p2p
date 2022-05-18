#![allow(dead_code, unused, warnings)]

extern crate lazy_static;

mod client;
mod gstreamer_actor;
mod pipeline;
mod sendrecv;
mod utils;
mod webrtc_actor;
mod webrtcbin_actor;

use anyhow::Result;
use bastion::prelude::*;
use webrtcbin_actor::{WebRTCBinActor, WebRTCBinActorType};

#[tokio::main]
async fn main() {
    Bastion::init();
    Bastion::start();

    let server_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(server_parent, WebRTCBinActorType::Server);
    // tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let client_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(client_parent, WebRTCBinActorType::Client);
    // tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // let server_parent = Bastion::supervisor(|s| s).unwrap();
    // sendrecv::test(server_parent, WebRTCBinActorType::Server);
    // // tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // let client_parent = Bastion::supervisor(|s| s).unwrap();
    // sendrecv::test(client_parent, WebRTCBinActorType::Client);
    // // tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    Bastion::block_until_stopped();
}

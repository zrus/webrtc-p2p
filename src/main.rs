#![allow(dead_code, unused, warnings)]

extern crate lazy_static;

mod client;
mod console_listener;
mod gstreamer_actor;
mod pipeline;
mod webrtc_actor;
mod webrtcbin_actor;
mod conn;

use anyhow::Result;
use bastion::prelude::*;
use webrtcbin_actor::{WebRTCBinActor, WebRTCBinActorType};

#[tokio::main]
async fn main() {
    Bastion::init();
    Bastion::start();

    let server_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(server_parent, WebRTCBinActorType::Server);

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client_parent = Bastion::supervisor(|s| s).unwrap();
    WebRTCBinActor::run(client_parent, WebRTCBinActorType::Client);

    Bastion::block_until_stopped();
}

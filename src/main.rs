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

    let start = 11;
    let end = 11;
    let room_id = 1234u16;

    for i in start..=end {
        if i == 3 {
            continue;
        }
        let server_parent = Bastion::supervisor(|s| s).unwrap();
        WebRTCBinActor::run(server_parent, WebRTCBinActorType::Server, i);

        let ws_server = Bastion::supervisor(|s| s).unwrap();
        WsActor::run(ws_server, i, room_id);
    }

    Bastion::block_until_stopped();
}

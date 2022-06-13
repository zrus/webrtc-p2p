use std::sync::Arc;

use bastion::{
    blocking, context::BastionContext, distributor::Distributor, message::MessageHandler, run,
    supervisor::SupervisorRef, Bastion,
};
use serde_json::json;
use tokio::select;

use crate::{web_socket::JsonMsg, webrtc_actor::WebRtcActor, webrtcbin_actor::SDPType};

pub struct NatsActor;

impl NatsActor {
    pub fn run(parent: SupervisorRef, num_of_cam: u8) {
        parent.supervisor(|s| {
            s.children(|c| {
                c.with_distributor(Distributor::named("nats_actor"))
                    .with_exec(move |ctx| executor(ctx, num_of_cam))
            })
        });
    }
}

async fn executor(ctx: BastionContext, num_of_cam: u8) -> Result<(), ()> {
    println!("nats running..");
    let nc = Arc::new(nats::asynk::connect("demo.nats.io").await.unwrap());
    nc.flush().await.unwrap();

    for i in 4..=num_of_cam {
        let cam_id = format!("cam_{i}");
        let sub = nc.subscribe(&cam_id).await.unwrap();
        blocking! {
            loop {
                println!("{cam_id}");
                if let Some(msg) = sub.next().await {
                    match serde_json::from_slice::<JsonMsg>(&msg.data) {
                      Ok(JsonMsg::Sdp { type_, sdp }) => {
                        let msg = json!({
                            "type": type_,
                            "sdp": sdp
                        });
                        if &type_ == "offer" {
                            let server_parent = Bastion::supervisor(|s| s).unwrap();
                            WebRtcActor::run(server_parent, &msg.to_string(), i);
                        }
                    }
                    Ok(JsonMsg::Ice { candidate, sdp_mline_index, sdp_mid }) => {
                        let msg = json!({
                            "candidate": candidate,
                            "sdp_mid": sdp_mid,
                            "sdp_mline_index": sdp_mline_index,
                            "username_fragment": String::new()
                        });
                        Distributor::named(format!("webrtc_{i}")).tell_one((candidate, sdp_mline_index, sdp_mid)).expect("couldn't send ICE to WebRTC actor");
                    }
                        Err(_) => continue
                    }
                }
            }
        };
    }

    loop {
        let conn = Arc::downgrade(&nc);
        MessageHandler::new(ctx.recv().await?)
      .on_tell(|(order, (sdp_type, sdp)): (u8, (SDPType, String)), _| {
        let conn = conn.clone();
        let msg = serde_json::to_vec(&JsonMsg::Sdp {
                  type_: sdp_type.to_str().to_owned(),
                  sdp,
              })
              .unwrap();
              let order = order + 10;
              println!("SEND answer to device_{order}");
              run! {
                let nc = conn.upgrade().unwrap();
                nc.publish(&format!("device_{order}"), msg).await.expect("could not publish answer to device"); 
              }
          })
          .on_tell(|(order, (sdp_mline_index, candidate, sdp_mid)): (u8, (u16, String, String)), _| {
            let conn = conn.clone();
                  let msg = serde_json::to_vec(&JsonMsg::Ice {
                      candidate,
                      sdp_mline_index,
                      sdp_mid,
                  })
                  .unwrap();
                  let order = order + 10;
                  println!("SEND ice to device_{order}");
                  run! {
                    let nc = conn.upgrade().unwrap();
                    nc.publish(&format!("device_{order}"), msg).await.expect("could not publish answer to device"); 
                  }
              },
          );
    }
}

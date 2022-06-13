use bastion::supervisor::{ActorRestartStrategy, RestartPolicy, RestartStrategy, SupervisorRef};
use gst::glib;

use crate::pipeline::Pipeline;

pub struct GstreamerActor;

impl GstreamerActor {
    pub fn run(parent: SupervisorRef, i: u8) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default().with_restart_policy(RestartPolicy::Never),
                )
                .children(|c| c.with_exec(move |_| async move { main_fn(i).await }))
            })
            .expect("couldn't run Gstreamer actor");
    }
}

async fn main_fn(i: u8) -> Result<(), ()> {
    println!("Gstreamer started");

    gst::init().expect("couldn't initialize gstreamer");

    let pipeline = Pipeline::init(i).expect("couldn't initialize pipeline");

    pipeline.run().expect("couldn't run pipeline on");

    loop {}
    Ok(())
}

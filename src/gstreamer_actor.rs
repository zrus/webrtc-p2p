use bastion::supervisor::{ActorRestartStrategy, RestartPolicy, RestartStrategy, SupervisorRef};
use gst::glib;

use crate::pipeline::Pipeline;

pub struct GstreamerActor;

impl GstreamerActor {
    pub fn run(parent: SupervisorRef) {
        parent
            .supervisor(|s| {
                s.with_restart_strategy(
                    RestartStrategy::default()
                        .with_restart_policy(RestartPolicy::Tries(5))
                        .with_actor_restart_strategy(ActorRestartStrategy::Immediate),
                )
                .children(|c| {
                    c.with_exec(|_| async {
                        let main_context = glib::MainContext::default();
                        main_context.block_on(main_fn());
                        loop {}
                    })
                })
            })
            .expect("couldn't run Gstreamer actor");
    }
}

async fn main_fn() {
    println!("Gstreamer started");

    gst::init().expect("couldn't initialize gstreamer");

    let pipeline = Pipeline::init().expect("couldn't initialize pipeline");

    pipeline.run().expect("couldn't run pipeline on");

    loop {}
}

use bastion::{message::MessageHandler, supervisor::SupervisorRef};

struct Client;

impl Client {
    pub fn run(parent: SupervisorRef) {
        parent
            .supervisor(|s| s.children(|c| c.with_exec(|ctx| async move { loop {} })))
            .expect("couldn't run Client actor");
    }
}

async fn main_fn() -> Result<(), ()> {
    Ok(())
}

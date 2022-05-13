use std::sync::{Arc, Weak};

use anyhow::bail;
use gst::{
    glib::{self},
    prelude::{Cast, ElementExt, ElementExtManual, GstObjectExt},
};

#[macro_export]
macro_rules! upgrade_weak {
    ($x:ident, $r:expr) => {{
        match $x.upgrade() {
            Some(o) => o,
            None => return $r,
        }
    }};
    ($x:ident) => {
        upgrade_weak!($x, ())
    };
}

#[derive(Debug, Clone)]
pub struct Pipeline(Arc<PipelineInner>);

#[derive(Debug, Clone)]
pub struct PipelineWeak(Weak<PipelineInner>);

#[derive(Debug)]
pub struct PipelineInner {
    pipeline: gst::Pipeline,
}

impl std::ops::Deref for Pipeline {
    type Target = PipelineInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for PipelineInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl PipelineWeak {
    fn upgrade(&self) -> Option<Pipeline> {
        self.0.upgrade().map(Pipeline)
    }
}

impl Pipeline {
    pub fn downgrade(&self) -> PipelineWeak {
        PipelineWeak(Arc::downgrade(&self.0))
    }
}

impl Pipeline {
    pub fn init() -> Result<Self, anyhow::Error> {
        let pipeline = gst::parse_launch(
            "videotestsrc pattern=ball is-live=true ! video/x-raw,width=640,height=480,format=I420 ! vp8enc error-resilient=partitions keyframe-max-dist=10 auto-alt-ref=true cpu-used=5 deadline=1 ! rtpvp8pay ! udpsink host=127.0.0.1 port=5004",
        )
        .expect("couldn't parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("couldn't downcast pipeline");

        let bus = pipeline.bus().unwrap();

        bus.add_watch_local(move |_, msg| {
            if handle_pipeline_msg(msg).is_err() {
                return glib::Continue(false);
            }
            glib::Continue(true)
        })
        .expect("couldn't add bus watch");

        Ok(Self(Arc::new(PipelineInner { pipeline })))
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
}

fn handle_pipeline_msg(msg: &gst::Message) -> Result<(), anyhow::Error> {
    use gst::message::MessageView;
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
        _ => (),
    }
    Ok(())
}

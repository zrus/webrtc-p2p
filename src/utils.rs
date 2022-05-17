use anyhow::bail;
use gst_sdp::SDPMessage;
use serde_json::{json, Value};

use crate::webrtcbin_actor::SDPType;

pub fn serialize(type_: &SDPType, sdp: &str) -> Result<String, anyhow::Error> {
    let mut json_answer = serde_json::to_string(sdp)?;
    let type_ = match type_ {
        gst_webrtc::WebRTCSDPType::Offer => "offer",
        // gst_webrtc::WebRTCSDPType::Pranswer => todo!(),
        gst_webrtc::WebRTCSDPType::Answer => "answer",
        // gst_webrtc::WebRTCSDPType::Rollback => todo!(),
        _ => bail!("SDP type not supported"),
    };

    json_answer = json!({
      "type": type_,
      "sdp": json_answer,
    })
    .to_string();
    let b64 = base64::encode(&json_answer);

    Ok(b64)
}

pub async fn deserialize(sdp: &str) -> Result<(SDPType, String), anyhow::Error> {
    let b64 = base64::decode(sdp)?;
    let json: Value = serde_json::from_slice(&b64)?;
    println!("json: {}", json);
    let ret = gst_sdp::SDPMessage::parse_buffer(&json["sdp"].as_str().unwrap().as_bytes())?;

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let type_ = match json["type"].as_str().unwrap() {
        "offer" => SDPType::Offer,
        "answer" => SDPType::Answer,
        _ => bail!("SDP type not supported"),
    };

    let sdp = json["sdp"].as_str().unwrap().to_owned();

    Ok((type_, sdp))
}

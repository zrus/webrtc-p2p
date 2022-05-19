# webrtc-p2p

## webrtcbin
Simulate 2 peers: 1 receive only, 1 send only. They are automatic sending offer and answer to the opposite.
Just run ```> cargo build --release --features=webrtcbin```
And then ```> ./target/release/webrtc-p2p```

## webrtc-rs
1. Go to https://jsfiddle.net/z7ms3u5r/ and copy the base64 SDP.
2. ```> cargo build --release --features=webrtc-rs```
3. ```> BROWSER=<SDP string that you just copied from test site>```
4. ```> echo $BROWSER | ./target/release/webrtc-p2p```
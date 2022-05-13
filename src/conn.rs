use std::sync::{Arc, Weak};

struct Connection(Arc<ConnectionInner>);

struct ConnectionWeak(Weak<ConnectionInner>);

struct ConnectionInner {
    peer_conn: Option<String>,
}

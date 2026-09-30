//! One viewer's link delay, from its frame acknowledgements. The logic is
//! shared with the agent, which tracks viewers on a direct path the same
//! way: see [`protocol::media::ViewerDelay`].

pub use protocol::media::{ViewerDelay, ACK_INTERVAL};

//! Screen streaming, platform-neutral parts.
//!
//! The Windows helper captures (DXGI) and encodes (Media Foundation) in the
//! user's session; everything here is shared with tests and other platforms:
//! pixel conversion, H.264 bitstream fix-ups, the channel types that
//! connect a frame source to the agent's server connection, and the
//! adaptive-bitrate controller.

pub mod h264;
pub mod nv12;
pub mod pace;
pub mod rate;
pub mod source;

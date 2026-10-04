//! How TetanusRMM looks, from the brand sheet: the palette, the light and
//! dark themes built on it, the mark (the T is a nail), the dialog icons,
//! and encoders for the forms they are needed in (`.ico`, RGBA, SVG).
//!
//! A company can put its own name, logo and accent colour in place of
//! TetanusRMM's (see [`theme::Theme`]); the layout, the type, the icons
//! and the colours that carry meaning (live red, the green checks) stay.
//!
//! Nothing here depends on a platform or another crate: the agent's build
//! script makes the executable's icon with it, the server draws the MSI's
//! icon and its web pages with it, and the Windows UI takes its colours
//! and shapes from it.

pub mod color;
pub mod ico;
pub mod icons;
pub mod mark;
pub mod path;
pub mod raster;
pub mod svg;
pub mod theme;
pub mod winres;

pub use color::Rgb;
pub use theme::Theme;

/// The product's name, and the agent's as its windows and tray show it.
pub const PRODUCT: &str = "TetanusRMM";
pub const AGENT_NAME: &str = "TetanusRMM Agent";

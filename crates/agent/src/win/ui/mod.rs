//! What the agent draws for the person at the machine, in the brand's
//! look (see the `brand` crate): the consent and lend-a-password dialogs,
//! quick assist's window, the session bar and the tray's flyout are all
//! built from these parts.
//!
//! - [`look`]: what to draw with right now: the light or dark theme, as
//!   Windows is set, and the company's branding if the server has one.
//! - [`canvas`]: drawing with Direct2D and DirectWrite (rounded shapes,
//!   the stroked icons, the mark, text), into a bitmap that a window then
//!   shows. GDI alone cannot draw any of that smoothly.
//! - [`dialog`]: a window with a native title bar, a body of a few kinds
//!   of block, and a footer of buttons, with the keyboard and mouse
//!   handling they need.
//! - [`about`]: the About box.
//! - [`preview`]: any of the surfaces with made-up data, to look at.
//!
//! Sizes are in device-independent pixels (96 to the inch) throughout, as
//! on the artboards; the canvas scales for the monitor.

pub mod about;
pub mod canvas;
pub mod dialog;
pub mod look;
pub mod preview;

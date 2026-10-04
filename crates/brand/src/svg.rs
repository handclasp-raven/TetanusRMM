//! The mark and the icons as SVG, for the server's web pages.

use crate::color::Rgb;
use crate::icons::{Icon, SIZE};
use crate::mark::{Mark, Variant};
use crate::path;

/// The mark, `px` pixels square, on a tile of `tile`.
pub fn mark(px: u32, tile: Rgb) -> String {
    let m = Mark::of(Variant::for_size(px));
    let (x, y, w, h, r) = m.bar;
    let stem: Vec<String> = m.stem.iter().map(|(x, y)| format!("{x} {y}")).collect();
    format!(
        "<svg width=\"{px}\" height=\"{px}\" viewBox=\"0 0 64 64\" aria-hidden=\"true\">\
         <rect width=\"64\" height=\"64\" rx=\"{}\" fill=\"{}\"/>\
         <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{r}\" fill=\"#FFFFFF\"/>\
         <path d=\"M{}z\" fill=\"#FFFFFF\"/></svg>",
        m.tile_radius,
        tile.to_hex(),
        stem.join("L"),
    )
}

/// The mark as a `data:` address, for a page's icon.
pub fn favicon(tile: Rgb) -> String {
    let svg = mark(32, tile).replace("<svg ", "<svg xmlns=\"http://www.w3.org/2000/svg\" ");
    // Only what a data address cannot hold as it is.
    let escaped = svg
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('"', "'")
        .replace('<', "%3C")
        .replace('>', "%3E");
    format!("data:image/svg+xml,{escaped}")
}

/// An icon, `px` pixels square, stroked in the page's `currentColor`.
pub fn icon(icon: &Icon, px: u32) -> String {
    let strokes: String = icon
        .outlines()
        .iter()
        .map(|(width, segs)| {
            format!(
                "<path d=\"{}\" stroke-width=\"{width}\"/>",
                path::to_svg(segs)
            )
        })
        .collect();
    format!(
        "<svg width=\"{px}\" height=\"{px}\" viewBox=\"0 0 {SIZE} {SIZE}\" fill=\"none\" \
         stroke=\"currentColor\" stroke-linecap=\"round\" stroke-linejoin=\"round\" \
         aria-hidden=\"true\">{strokes}</svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons;
    use crate::theme::palette;

    #[test]
    fn the_mark_is_the_sheets_drawing() {
        let large = mark(72, palette::RUST);
        assert!(large.contains("rx=\"14\" fill=\"#B5441C\""));
        assert!(large.contains("x=\"14\" y=\"13\" width=\"36\" height=\"9\" rx=\"3\""));
        assert!(large.contains("M27.5 22L36.5 22L36.5 41L32 52L27.5 41z"));
        // Small sizes get the heavier one.
        assert!(mark(16, palette::RUST).contains("width=\"44\" height=\"13\""));
    }

    #[test]
    fn the_favicon_is_a_self_contained_address() {
        let uri = favicon(palette::RUST);
        assert!(uri.starts_with("data:image/svg+xml,%3Csvg xmlns="));
        assert!(uri.contains("%23B5441C") && !uri.contains('"') && !uri.contains('#'));
    }

    #[test]
    fn icons_take_the_pages_colour() {
        let svg = icon(&icons::SAFETY, 28);
        assert!(svg.contains("stroke=\"currentColor\"") && svg.contains("stroke-width=\"1.75\""));
        assert_eq!(svg.matches("<path").count(), 2);
    }
}

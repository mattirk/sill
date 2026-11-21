//! `cargo run -p silli-gui --example mac_icon -- SVG OUT.iconset`: the Mac app's icon set from an
//! SVG (packaging/mac/app.sh makes Silli.icns of it with iconutil). The artwork sits in macOS's
//! icon grid: 824 of 1024 pixels, centred, so it is as large as other apps' in the Dock.
use resvg::{tiny_skia, usvg};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(svg), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: mac_icon SVG OUT.iconset".into());
    };
    let tree = usvg::Tree::from_str(&std::fs::read_to_string(svg)?, &usvg::Options::default())?;
    std::fs::create_dir_all(&out)?;
    for (points, scale) in [(16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2), (256, 1), (256, 2), (512, 1), (512, 2)] {
        let size = points * scale;
        let mut canvas = tiny_skia::Pixmap::new(size, size).ok_or("canvas")?;
        let art = size as f32 * 824.0 / 1024.0;
        let fit = art / tree.size().width().max(tree.size().height());
        let offset = (size as f32 - art) / 2.0;
        resvg::render(&tree, tiny_skia::Transform::from_scale(fit, fit).post_translate(offset, offset), &mut canvas.as_mut());
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for pixel in canvas.pixels() {
            let c = pixel.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        let name = if scale == 1 { format!("icon_{points}x{points}.png") } else { format!("icon_{points}x{points}@2x.png") };
        image::RgbaImage::from_raw(size, size, rgba).ok_or("pixels")?.save(std::path::Path::new(&out).join(name))?;
    }
    Ok(())
}

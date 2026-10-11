//! Embeds `Icon/EMU Assistant.png` as the exe icon (multi-size .ico) plus version info.

use std::path::PathBuf;

const ICON: &str = "Icon/EMU Assistant.png";

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.ico");
    if let Err(e) = write_ico(&out) {
        println!("cargo:warning=could not build app icon: {e}");
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().unwrap())
        .set("ProductName", "EMU Assistant")
        .set("FileDescription", "EMU Assistant")
        .set("InternalName", "EMUAssistant");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed app icon (is windres on PATH?): {e}");
    }
}

fn write_ico(out: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let src = image::open(ICON)?.to_rgba8();
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let img = image::imageops::resize(&src, size, size, image::imageops::FilterType::Lanczos3);
        let icon = ico::IconImage::from_rgba_data(size, size, img.into_raw());
        dir.add_entry(ico::IconDirEntry::encode(&icon)?);
    }
    dir.write(std::fs::File::create(out)?)?;
    Ok(())
}

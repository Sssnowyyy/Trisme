//! sets hw_raster when a host GPU rasterizer is built in, Vulkan's or the
//! Wii U's GX2 one, for the code both share with the software path.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_VULKAN").is_some() || std::env::var_os("CARGO_FEATURE_GX2").is_some() {
        println!("cargo::rustc-cfg=hw_raster");
    }
}

// Tells the core apart from the target name: rustc has no cfg for "has MSPLIM".
fn main() {
    println!("cargo::rustc-check-cfg=cfg(unprobe_v8m)");
    println!("cargo::rustc-check-cfg=cfg(unprobe_v6m)");
    println!("cargo::rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.starts_with("thumbv8m.main") {
        // RP2350 Arm cores (Cortex-M33)
        println!("cargo::rustc-cfg=unprobe_v8m");
    } else if target.starts_with("thumbv6m") {
        // RP2040 (Cortex-M0+)
        println!("cargo::rustc-cfg=unprobe_v6m");
    }
}

use std::env;

fn main() {
    if env::var_os("CARGO_FEATURE_XDP").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }

    let root_dir = format!("{}/../vata-ebpf", env!("CARGO_MANIFEST_DIR"));
    aya_build::build_ebpf(
        [aya_build::Package {
            name: "vata-ebpf",
            root_dir: &root_dir,
            no_default_features: false,
            features: &[],
        }],
        aya_build::Toolchain::default(),
    )
    .expect("failed to build vata-ebpf");
}

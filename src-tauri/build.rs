use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=native/video_toolbox_encoder.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_video_toolbox_shim();
    }
    tauri_build::build()
}

fn build_video_toolbox_shim() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let archive = out_dir.join("libgolive_videotoolbox.a");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture is set");
    let swift_target = format!("{arch}-apple-macosx12.3");
    let sdk = String::from_utf8(
        Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .expect("failed to locate the macOS SDK")
            .stdout,
    )
    .expect("macOS SDK path is valid UTF-8")
    .trim()
    .to_owned();
    let swiftc = PathBuf::from(
        String::from_utf8(
            Command::new("xcrun")
                .args(["--find", "swiftc"])
                .output()
                .expect("failed to locate swiftc")
                .stdout,
        )
        .expect("swiftc path is valid UTF-8")
        .trim(),
    );
    let swift_runtime = swiftc
        .parent()
        .and_then(|path| path.parent())
        .expect("swiftc is installed below a toolchain")
        .join("lib/swift/macosx");
    let status = Command::new("xcrun")
        .args([
            "swiftc",
            "-swift-version",
            "6",
            "-parse-as-library",
            "-target",
            &swift_target,
            "-emit-library",
            "-static",
            "-o",
        ])
        .arg(&archive)
        .arg("native/video_toolbox_encoder.swift")
        .status()
        .expect("failed to invoke xcrun swiftc for VideoToolbox shim");
    assert!(
        status.success(),
        "VideoToolbox Swift shim compilation failed"
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-search=native={}/usr/lib/swift", sdk);
    println!("cargo:rustc-link-search=native={}", swift_runtime.display());
    println!("cargo:rustc-link-lib=static=golive_videotoolbox");
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=static=swiftCompatibility56");
    println!("cargo:rustc-link-lib=static=swiftCompatibilityPacks");
    for framework in ["CoreMedia", "CoreVideo", "Foundation", "VideoToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

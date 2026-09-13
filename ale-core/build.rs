fn main() {
    println!("cargo:rerun-if-env-changed=ALE_BUILD_ID");
    println!(
        "cargo:rustc-env=ALE_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
    println!(
        "cargo:rustc-env=ALE_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap()
    );
    let id = std::env::var("ALE_BUILD_ID")
        .ok()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "development".into());
    println!("cargo:rustc-env=ALE_BUILD_ID={id}");
}

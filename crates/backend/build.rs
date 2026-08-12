use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let lock_path = manifest_dir.join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock = std::fs::read_to_string(&lock_path).unwrap_or_default();
    for (package, variable) in [
        ("furumi-library", "FURUMI_LIBRARY_VERSION"),
        ("music-dht", "FURUMI_MUSIC_DHT_VERSION"),
        ("federation-net", "FURUMI_FEDERATION_NET_VERSION"),
    ] {
        println!(
            "cargo:rustc-env={variable}={}",
            package_version(&lock, package).unwrap_or("unknown")
        );
    }
}

fn package_version<'a>(lock: &'a str, package: &str) -> Option<&'a str> {
    lock.split("[[package]]").find_map(|section| {
        let mut name = None;
        let mut version = None;
        for line in section.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix("name = \"") {
                name = value.strip_suffix('"');
            } else if let Some(value) = line.strip_prefix("version = \"") {
                version = value.strip_suffix('"');
            }
        }
        (name == Some(package)).then_some(version).flatten()
    })
}

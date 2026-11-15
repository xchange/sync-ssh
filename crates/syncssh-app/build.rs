fn main() {
    println!("cargo:rerun-if-changed=assets/icons/SyncSSH.ico");
    println!("cargo:rerun-if-changed=assets/windows.rc");
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use std::{env, fs, path::PathBuf};
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        let numbers: Vec<u16> = version
            .split('.')
            .map(|part| part.parse().expect("Expected numeric application version"))
            .collect();
        assert_eq!(
            numbers.len(),
            3,
            "Expected a three-part application version"
        );
        let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let icon = manifest
            .join("assets/icons/SyncSSH.ico")
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(manifest.join("assets/windows.rc"))
            .unwrap()
            .replace("@SYNCSSH_ICON@", &icon)
            .replace(
                "@SYNCSSH_VERSION_NUMBERS@",
                &format!("{},{},{},0", numbers[0], numbers[1], numbers[2]),
            )
            .replace("@SYNCSSH_VERSION@", &version);
        let resource = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("syncssh.rc");
        fs::write(&resource, source).unwrap();
        embed_resource::compile(resource, embed_resource::NONE)
            .manifest_required()
            .expect("Unable to embed SyncSSH icon and version resources");
    }
}

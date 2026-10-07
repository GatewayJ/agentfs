fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        assert_eq!(std::env::var("CARGO_CFG_TARGET_ENV").as_deref(), Ok("msvc"));
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture");
        let suffix = match arch.as_str() {
            "x86_64" => "x64",
            "x86" => "x86",
            "aarch64" => "a64",
            _ => panic!("unsupported WinFsp architecture"),
        };
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-{suffix}.dll");
    }
}

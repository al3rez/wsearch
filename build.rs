fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/Assets/wsearch.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/MANIFESTUAC:level='asInvoker'");
        println!("cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
        embed_icon();
    }
}

fn embed_icon() {
    let rc_exe = r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\rc.exe";
    if !std::path::Path::new(rc_exe).is_file() {
        return;
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = std::path::Path::new(&manifest).join("ui/Assets/wsearch.ico");
    if !ico.is_file() {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    let rc_path = std::path::Path::new(&out).join("wsearch.rc");
    let res_path = std::path::Path::new(&out).join("wsearch.res");
    let ico_s = ico.to_string_lossy().replace('\\', "/");
    std::fs::write(&rc_path, format!("1 ICON \"{ico_s}\"\r\n")).expect("write rc");
    let ok = std::process::Command::new(rc_exe).args(["/nologo", "/fo"]).arg(&res_path).arg(&rc_path).status().map(|s| s.success()).unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg={}", res_path.display());
    }
}

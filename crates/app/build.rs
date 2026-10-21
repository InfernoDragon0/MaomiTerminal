//! Embeds the app icon in the Windows exe as icon resource 1, which GPUI loads for its windows
//! and Explorer / the taskbar show for the file.

const ICON: &str = "../../packaging/icon/icon.ico";

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    // The build script runs on the host, so check the target, not cfg!(windows).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ICON);
        if let Err(err) = res.compile() {
            panic!("could not embed the Windows icon: {err}");
        }
    }
}

fn main() {
    // The ScreenCaptureKit bridge is Swift; its runtime (libswift_Concurrency) lives in /usr/lib/swift.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    tauri_build::build()
}

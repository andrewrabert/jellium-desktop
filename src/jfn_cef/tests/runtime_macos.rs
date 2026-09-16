#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use jfn_cef::{LoadError, LoadedCef};
    let framework = std::path::PathBuf::from(
        std::env::var_os("JFN_CEF_TEST_FRAMEWORK")
            .ok_or("set JFN_CEF_TEST_FRAMEWORK to the native CEF framework binary")?,
    );
    let worker_path = framework.clone();
    let rejected = std::thread::spawn(move || {
        matches!(
            LoadedCef::load_framework(&worker_path),
            Err(LoadError::WrongThread)
        )
    })
    .join()
    .map_err(|_| "loading worker panicked")?;
    assert!(rejected, "framework loading must reject worker threads");

    let (version, formatted) = {
        let runtime = LoadedCef::load_framework(&framework)?;
        let version = runtime.version().clone();
        assert!(version.major > 0);
        assert!(version.chromium[0] > 0);
        let formatted = version.to_string();
        (version, formatted)
    };
    unsafe extern "C" {
        fn cef_version_info(entry: std::ffi::c_int) -> std::ffi::c_int;
    }
    assert_eq!(
        unsafe { cef_version_info(0) },
        i32::try_from(version.major)?
    );
    assert_eq!(version.to_string(), formatted);
    assert!(matches!(
        LoadedCef::load_framework(&framework),
        Err(LoadError::BootstrapAlreadyClaimed)
    ));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}

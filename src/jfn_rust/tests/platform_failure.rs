#[cfg(target_os = "linux")]
#[test]
fn failed_backend_returns_its_cleanup_owner() -> Result<(), Box<dyn std::error::Error>> {
    use jfn_platform_abi::{PlatformInitError, PreparedPlatform};
    jfn_platform_abi::install(jfn_x11::make_platform::make_x11_platform());
    let prepared = PreparedPlatform::claim(unsafe { jfn_platform_abi::get() })?;
    let (error, prepared) = match prepared.initialize(std::ptr::null_mut()) {
        Ok(runtime) => {
            if let Err(error) = runtime.cleanup(|| Ok::<(), std::convert::Infallible>(())) {
                match error {
                    jfn_platform_abi::PlatformCleanupError::Busy { runtime, terminate } => {
                        let _ = Box::leak(Box::new((runtime, terminate)));
                        return Err("unexpected busy platform".into());
                    }
                    jfn_platform_abi::PlatformCleanupError::Termination { error, .. } => {
                        match error {}
                    }
                }
            }
            return Err("initialization unexpectedly succeeded without a host window".into());
        }
        Err(failure) => failure,
    };
    assert!(matches!(
        error,
        PlatformInitError::Backend {
            operation: "X11 host acquisition",
            ..
        }
    ));
    assert!(std::error::Error::source(&error).is_some());
    let mut terminated = false;
    if let Err((never, _)) = prepared.cleanup(|| {
        terminated = true;
        Ok::<(), std::convert::Infallible>(())
    }) {
        match never {}
    }
    assert!(
        terminated,
        "failure must preserve ordered window termination"
    );
    assert!(matches!(
        PreparedPlatform::claim(unsafe { jfn_platform_abi::get() }),
        Err(PlatformInitError::AlreadyClaimed)
    ));
    Ok(())
}

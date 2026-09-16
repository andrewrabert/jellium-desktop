fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(target: "panic", "PANIC: {info}\n{bt}");
        eprintln!("PANIC: {info}\n{bt}");
        default_hook(info);
    }));

    std::process::exit(jfn_rust::app::jfn_app_main());
}

//! `capture-proxy` entrypoint.
//!
//! Deliberately thin: everything except reading `argv` and setting the exit status lives in
//! [`capture_proxy::app`], so the whole start-up sequence is reachable from a test without
//! spawning a process.

fn main() {
    // `args_os` would be more faithful, but every downstream consumer — clap, the inline-JSON
    // decoder, the configuration model — is `String`-typed, so a non-UTF-8 argument has to be
    // rejected somewhere. Rejecting it here, by name, beats a lossy conversion that silently
    // mangles a certificate path.
    let mut raw_args = Vec::new();
    for argument in std::env::args_os().skip(1) {
        match argument.into_string() {
            Ok(argument) => raw_args.push(argument),
            Err(argument) => {
                eprintln!(
                    "capture-proxy: arguments must be valid UTF-8; {} is not",
                    argument.to_string_lossy()
                );
                std::process::exit(capture_config::CONFIG_ERROR_EXIT_CODE);
            }
        }
    }

    std::process::exit(capture_proxy::app::run(&raw_args));
}

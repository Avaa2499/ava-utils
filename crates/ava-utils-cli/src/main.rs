use ava_utils_cli::app::dispatch;
use ava_utils_cli::install::{Layout, SocketProbe};
use ava_utils_cli::systemd::RealRunner;
use ava_utils_config::error::{Error, codes};

fn main() {
    let result = Layout::from_env().and_then(|layout| {
        let probe = SocketProbe(layout.dirs.clone());
        let cwd = std::env::current_dir()?;
        dispatch(
            std::env::args().skip(1).collect(),
            &layout,
            &RealRunner,
            &probe,
            &cwd,
        )
    });
    match result {
        Ok(text) => println!("{text}"),
        Err(e) => {
            match &e {
                Error::Remote { code, message }
                    if code == codes::BAD_REQUEST || code == codes::FAILED =>
                {
                    eprintln!("au: {message}")
                }
                e => eprintln!("au: {e}"),
            }
            std::process::exit(1);
        }
    }
}

use ava_utils_cli::app::dispatch;
use ava_utils_cli::install::{Layout, SocketProbe};
use ava_utils_cli::systemd::RealRunner;

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
            eprintln!("au: {e}");
            std::process::exit(1);
        }
    }
}

pub fn run_command(args: &[String], pid: u32) -> String {
    match args.first().map(String::as_str) {
        None => "toy is up. commands: ping, echo".to_string(),
        Some("ping") => format!("pong pid={pid}"),
        Some("echo") => args[1..].join(" "),
        Some(other) => format!("unknown command: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_arguments_says_it_is_up() {
        assert!(run_command(&[], 1).contains("up"));
    }

    #[test]
    fn ping_reports_the_pid() {
        assert_eq!(run_command(&a(&["ping"]), 42), "pong pid=42");
    }

    #[test]
    fn echo_repeats_the_arguments() {
        assert_eq!(run_command(&a(&["echo", "a", "b"]), 1), "a b");
    }

    #[test]
    fn unknown_commands_are_reported() {
        assert_eq!(run_command(&a(&["nope"]), 1), "unknown command: nope");
    }
}

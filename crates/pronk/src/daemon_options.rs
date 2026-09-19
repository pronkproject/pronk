//! Explicit daemon settings parsed before acquiring service or device authority.

use std::ffi::{OsStr, OsString};

pub const USAGE: &str = "usage: pronkd\n       pronkd --version\n       pronkd --help";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run,
    Version,
    Help,
}

pub fn parse(arguments: &[OsString]) -> Result<Command, &'static str> {
    match arguments {
        [] => Ok(Command::Run),
        [flag] if flag == OsStr::new("--version") => Ok(Command::Version),
        [flag] if flag == OsStr::new("--help") => Ok(Command::Help),
        _ => Err(USAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<Command, &'static str> {
        parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_start_the_service() {
        assert_eq!(parse_words(&[]), Ok(Command::Run));
    }

    #[test]
    fn information_commands_do_not_request_a_service_start() {
        assert_eq!(parse_words(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_words(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn unknown_missing_and_repeated_settings_are_rejected() {
        for words in [
            vec!["--capture-source", "renderer"],
            vec!["--capture-source", "final-image"],
            vec!["--unknown"],
        ] {
            assert_eq!(parse_words(&words), Err(USAGE));
        }
    }
}

//! Explicit daemon settings parsed before acquiring service or device authority.

use std::ffi::{OsStr, OsString};

use pronk::display_media::CaptureSource;

pub const USAGE: &str = "usage: pronkd [--capture-source renderer|final-image]\n       pronkd --version\n       pronkd --help";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run { capture_source: CaptureSource },
    Version,
    Help,
}

pub fn parse(arguments: &[OsString]) -> Result<Command, &'static str> {
    match arguments {
        [] => Ok(Command::Run {
            capture_source: CaptureSource::default(),
        }),
        [flag] if flag == OsStr::new("--version") => Ok(Command::Version),
        [flag] if flag == OsStr::new("--help") => Ok(Command::Help),
        [flag, source] if flag == OsStr::new("--capture-source") => {
            let capture_source = match source.to_str() {
                Some("renderer") => CaptureSource::Renderer,
                Some("final-image") => CaptureSource::FinalImage,
                _ => return Err(USAGE),
            };
            Ok(Command::Run { capture_source })
        }
        _ => Err(USAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn parse_words(words: &[&str]) -> Result<Command, &'static str> {
        parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn capture_source_is_explicit_and_renderer_remains_the_default() {
        assert_eq!(
            parse_words(&[]),
            Ok(Command::Run {
                capture_source: CaptureSource::Renderer,
            })
        );
        assert_eq!(
            parse_words(&[]),
            parse_words(&["--capture-source", "renderer"])
        );
        assert_eq!(
            parse_words(&["--capture-source", "final-image"]),
            Ok(Command::Run {
                capture_source: CaptureSource::FinalImage,
            })
        );
    }

    #[test]
    fn information_commands_do_not_request_a_service_start() {
        assert_eq!(parse_words(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_words(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn unknown_missing_and_repeated_settings_are_rejected() {
        for words in [
            vec!["--capture-source"],
            vec!["--capture-source", "automatic"],
            vec!["--capture-source", "final-image", "--version"],
            vec![
                "--capture-source",
                "renderer",
                "--capture-source",
                "final-image",
            ],
            vec!["--unknown"],
        ] {
            assert_eq!(parse_words(&words), Err(USAGE));
        }
        assert_eq!(
            parse(&["--capture-source".into(), OsString::from_vec(vec![0xff])]),
            Err(USAGE)
        );
    }
}

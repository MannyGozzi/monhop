//! The one accepted startup argument, if any.

use std::ffi::OsString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    Normal,
    CheckUi,
    Login,
}

impl Launch {
    /// Anything but exactly one recognized argument is a normal launch.
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Self {
        let mut arguments = arguments.into_iter();
        let Some(first) = arguments.next() else {
            return Self::Normal;
        };
        if arguments.next().is_some() {
            return Self::Normal;
        }
        if first == "--check-ui" {
            Self::CheckUi
        } else if first == "--login" {
            Self::Login
        } else {
            Self::Normal
        }
    }
}

/// `--check-island` alone runs only the notch island's check; elsewhere it stays a normal launch.
#[cfg(any(target_os = "macos", test))]
pub fn island_check(arguments: impl IntoIterator<Item = OsString>) -> bool {
    let mut arguments = arguments.into_iter();
    cfg!(target_os = "macos")
        && arguments
            .next()
            .is_some_and(|first| first == "--check-island")
        && arguments.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_is_a_normal_launch() {
        assert_eq!(Launch::parse([]), Launch::Normal);
    }

    #[test]
    fn each_recognized_argument_is_the_only_accepted_form() {
        assert_eq!(Launch::parse(["--check-ui".into()]), Launch::CheckUi);
        assert_eq!(Launch::parse(["--login".into()]), Launch::Login);
    }

    #[test]
    fn anything_else_or_extra_arguments_is_a_normal_launch() {
        for arguments in [
            vec!["--check-ui".into(), "extra".into()],
            vec!["--login".into(), "extra".into()],
            vec!["--check-ui=yes".into()],
            vec!["--unknown".into()],
            vec!["--check-island".into()],
        ] {
            assert_eq!(Launch::parse(arguments), Launch::Normal);
        }
    }

    #[test]
    fn the_island_check_is_one_exact_argument_and_macos_only() {
        assert_eq!(
            island_check(["--check-island".into()]),
            cfg!(target_os = "macos")
        );
        for arguments in [
            vec![],
            vec!["--check-island".into(), "extra".into()],
            vec!["--check-island=yes".into()],
            vec!["--check-ui".into()],
        ] {
            assert!(!island_check(arguments));
        }
    }
}

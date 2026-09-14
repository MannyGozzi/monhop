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
        ] {
            assert_eq!(Launch::parse(arguments), Launch::Normal);
        }
    }
}

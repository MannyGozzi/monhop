//! The only web pages MonHop opens, as fixed constants. The window names a target, never an address.

const SOURCE_URL: &str = "https://github.com/MannyGozzi/monhop";
const RELEASES_URL: &str = "https://github.com/MannyGozzi/monhop/releases";
const SUPPORT_URL: &str = "https://github.com/sponsors/MannyGozzi";
const UNKNOWN_TARGET: &str = "MonHop opens only its own source, release notes and support pages.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Source,
    Releases,
    Support,
}

impl Link {
    /// The allowlist: anything the window asks for that is not one of these three is refused.
    fn parse(target: &str) -> Result<Self, String> {
        match target {
            "source" => Ok(Self::Source),
            "releases" => Ok(Self::Releases),
            "support" => Ok(Self::Support),
            _ => Err(UNKNOWN_TARGET.to_owned()),
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Source => SOURCE_URL,
            Self::Releases => RELEASES_URL,
            Self::Support => SUPPORT_URL,
        }
    }
}

fn unopened(link: Link) -> String {
    format!("The browser did not open. Visit {} yourself.", link.url())
}

pub fn open(link: Link) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::{NSString, NSURL};
        let url =
            NSURL::URLWithString(&NSString::from_str(link.url())).ok_or_else(|| unopened(link))?;
        if NSWorkspace::sharedWorkspace().openURL(&url) {
            Ok(())
        } else {
            Err(unopened(link))
        }
    }
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{HSTRING, PCWSTR, w},
        };
        let file = HSTRING::from(link.url());
        // SAFETY: Both strings outlive the call, which hands the shell one fixed https address.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                &file,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err(unopened(link))
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(unopened(link))
    }
}

/// Opens one of the three fixed pages in the default browser. Runs on the UI thread.
#[tauri::command]
pub fn app_open_link(target: String) -> Result<(), String> {
    open(Link::parse(&target)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_known_target_maps_to_its_own_fixed_page() {
        assert_eq!(Link::parse("source").unwrap(), Link::Source);
        assert_eq!(Link::parse("releases").unwrap(), Link::Releases);
        assert_eq!(Link::parse("support").unwrap(), Link::Support);
        assert_eq!(Link::Source.url(), "https://github.com/MannyGozzi/monhop");
        assert_eq!(
            Link::Releases.url(),
            "https://github.com/MannyGozzi/monhop/releases"
        );
        assert_eq!(
            Link::Support.url(),
            "https://github.com/sponsors/MannyGozzi"
        );
        for link in [Link::Source, Link::Releases, Link::Support] {
            assert!(link.url().starts_with("https://github.com/"));
        }
    }

    #[test]
    fn anything_but_the_three_targets_is_refused() {
        for target in [
            "",
            "Source",
            "https://example.com",
            "source ",
            "sponsors",
            "source/../releases",
        ] {
            assert_eq!(Link::parse(target).unwrap_err(), UNKNOWN_TARGET);
        }
    }
}

use std::{net::Ipv4Addr, time::Duration};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Help,
    Displays,
    Permissions,
    RequestPermissions,
    Interfaces,
    ShowIdentity,
    CreateIdentity,
    Capture(Duration),
    CheckPeer {
        interface: u32,
        local: Ipv4Addr,
        peer: Ipv4Addr,
    },
}

pub const HELP: &str = "MonHop native diagnostics (sharing disabled)

Usage:
  monhop                       Show current safety status
  monhop displays              Enumerate attached displays
  monhop permissions           Read native input permission status
  monhop permissions --request Ask macOS for standard input permissions
  monhop interfaces            List IPv4 adapter metadata (Windows/macOS)
  monhop identity --show        Read the protected identity's public fingerprint
  monhop identity --create      Create an OS-protected identity, never replace one
  monhop capture --seconds N   Count passive input events for 1..30 seconds
  monhop check-peer --interface INDEX --local IPv4 --peer IPv4
                                Validate the selected route without connecting

No command here enables remote control, local input suppression, or startup.
Capture counts categories only. It does not record or print keys or text.
Identity actions may show a standard macOS Keychain dialog. They do not pair a peer.";

pub fn parse(args: &[String]) -> Result<Command, &'static str> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["status"] => Ok(Command::Status),
        ["--help"] | ["-h"] | ["help"] => Ok(Command::Help),
        ["displays"] => Ok(Command::Displays),
        ["permissions"] => Ok(Command::Permissions),
        ["permissions", "--request"] => Ok(Command::RequestPermissions),
        ["interfaces"] => Ok(Command::Interfaces),
        ["identity", "--show"] => Ok(Command::ShowIdentity),
        ["identity", "--create"] => Ok(Command::CreateIdentity),
        ["capture", "--seconds", seconds] => {
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| "Capture requires a whole number of seconds")?;
            if !(1..=30).contains(&seconds) {
                return Err("Capture duration must be 1..30 seconds");
            }
            Ok(Command::Capture(Duration::from_secs(seconds)))
        }
        [
            "check-peer",
            "--interface",
            index,
            "--local",
            local,
            "--peer",
            peer,
        ] => {
            let interface = index
                .parse::<u32>()
                .map_err(|_| "A numeric interface index is required")?;
            if interface == 0 {
                return Err("Interface zero is not allowed");
            }
            Ok(Command::CheckPeer {
                interface,
                local: local
                    .parse()
                    .map_err(|_| "Local address must be numeric IPv4; no DNS or IPv6")?,
                peer: peer
                    .parse()
                    .map_err(|_| "Peer address must be numeric IPv4; no DNS or IPv6")?,
            })
        }
        _ => Err("Unknown or incomplete command. Run monhop --help"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(value: &str) -> Vec<String> {
        value.split_whitespace().map(str::to_owned).collect()
    }
    #[test]
    fn startup_is_inert_and_capture_must_be_bounded() {
        assert_eq!(parse(&[]), Ok(Command::Status));
        for command in [
            "capture",
            "capture --seconds 0",
            "capture --seconds 31",
            "capture --seconds -1",
            "capture --seconds 1.5",
        ] {
            assert!(parse(&args(command)).is_err());
        }
        assert_eq!(
            parse(&args("capture --seconds 1")),
            Ok(Command::Capture(Duration::from_secs(1)))
        );
    }
    #[test]
    fn no_implicit_dns_interface_or_control_command() {
        for command in [
            "check-peer --interface 0 --local 192.168.1.2 --peer 192.168.1.3",
            "check-peer --interface 7 --local 192.168.1.2 --peer example.com",
            "check-peer --interface 7 --local 192.168.1.2 --peer ::1",
            "connect",
            "listen",
            "inject",
            "enable",
            "install-startup",
        ] {
            assert!(parse(&args(command)).is_err());
        }
    }

    #[test]
    fn identity_actions_require_one_exact_explicit_operation() {
        assert_eq!(parse(&args("identity --show")), Ok(Command::ShowIdentity));
        assert_eq!(
            parse(&args("identity --create")),
            Ok(Command::CreateIdentity)
        );
        for command in [
            "identity",
            "identity --create --show",
            "identity --replace",
            "identity --export-private",
            "identity --create --force",
            "identity --show extra",
        ] {
            assert!(parse(&args(command)).is_err());
        }
    }
}

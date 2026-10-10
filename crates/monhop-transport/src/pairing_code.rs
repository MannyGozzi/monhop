//! The short pairing code: the showing computer's low address bits and a fresh secret, typed on
//! the other computer. The code grants nothing by itself; only its confirmation over TLS does.

use std::{fmt, net::Ipv4Addr};

use ring::rand::{SecureRandom, SystemRandom};
use zeroize::{Zeroize, Zeroizing};

use crate::policy::validate_subnet_peer;

pub const PAIRING_CODE_SYMBOLS: usize = 12;
/// Longer input is refused before it is normalized.
pub const MAX_PAIRING_CODE_INPUT_BYTES: usize = 64;
const GROUP_SYMBOLS: usize = 4;
const SYMBOL_BITS: u32 = 5;
const ADDRESS_BITS: u32 = 16;
const SECRET_BITS: u32 = 39;
const DATA_BITS: u32 = ADDRESS_BITS + SECRET_BITS;
const CHECK_BITS: u32 = 5;
const _: () = assert!(DATA_BITS + CHECK_BITS == SYMBOL_BITS * PAIRING_CODE_SYMBOLS as u32);
/// Crockford base32: no I, L, O or U, which read as other symbols.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// x^5 + x^2 + 1. Its constant term makes it catch every error burst of up to five bits, so every
/// single-symbol substitution.
const CHECK_GENERATOR: u64 = 0b10_0101;
const PASSWORD_PREFIX: &[u8] = b"monhop-pair-v1";
const DATA_BYTES: usize = 7;
const _: () = assert!(DATA_BITS as usize <= DATA_BYTES * 8);
pub(crate) const PASSWORD_BYTES: usize = PASSWORD_PREFIX.len() + DATA_BYTES;

/// A code's display form, wiped when dropped.
pub type ShownCode = Zeroizing<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingCodeError {
    Incomplete,
    Mistyped,
    OffNetwork,
    Unavailable,
}

impl fmt::Display for PairingCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Incomplete => "Enter the 12 letters and digits the other computer shows.",
            Self::Mistyped => {
                "That code has a typo. Check each character against the other computer."
            }
            Self::OffNetwork => "That code does not point to a computer on this network.",
            Self::Unavailable => "A code could not be made on this computer. Try again.",
        })
    }
}

impl std::error::Error for PairingCodeError {}

/// The 55 data bits: the showing computer's low 16 address bits, then a 39-bit secret.
pub struct PairingCode {
    data: u64,
}

impl PairingCode {
    /// A fresh code for a computer showing it at `showing`; the secret comes from the OS CSPRNG.
    pub fn generate(showing: Ipv4Addr) -> Result<Self, PairingCodeError> {
        let mut random = Zeroizing::new([0_u8; 8]);
        SystemRandom::new()
            .fill(random.as_mut_slice())
            .map_err(|_| PairingCodeError::Unavailable)?;
        let secret = u64::from_be_bytes(*random) & mask(SECRET_BITS);
        Ok(Self {
            data: (u64::from(u32::from(showing)) & mask(ADDRESS_BITS)) << SECRET_BITS | secret,
        })
    }

    /// Case-insensitive; dashes and whitespace are ignored, O reads as 0 and I or L as 1.
    pub fn parse(input: &str) -> Result<Self, PairingCodeError> {
        if input.len() > MAX_PAIRING_CODE_INPUT_BYTES {
            return Err(PairingCodeError::Incomplete);
        }
        let mut value = Zeroizing::new(0_u64);
        let mut symbols = 0;
        for byte in input.bytes() {
            if byte == b'-' || byte.is_ascii_whitespace() {
                continue;
            }
            let symbol =
                symbol_value(byte.to_ascii_uppercase()).ok_or(PairingCodeError::Incomplete)?;
            symbols += 1;
            if symbols > PAIRING_CODE_SYMBOLS {
                return Err(PairingCodeError::Incomplete);
            }
            *value = *value << SYMBOL_BITS | symbol;
        }
        if symbols != PAIRING_CODE_SYMBOLS {
            return Err(PairingCodeError::Incomplete);
        }
        let code = Self {
            data: *value >> CHECK_BITS,
        };
        if check(code.data) != *value & mask(CHECK_BITS) {
            return Err(PairingCodeError::Mistyped);
        }
        Ok(code)
    }

    /// `XXXX-XXXX-XXXX`, for the showing computer's screen only.
    pub fn display(&self) -> ShownCode {
        let value = Zeroizing::new(self.data << CHECK_BITS | check(self.data));
        let mut text = Zeroizing::new(String::with_capacity(
            PAIRING_CODE_SYMBOLS + PAIRING_CODE_SYMBOLS / GROUP_SYMBOLS,
        ));
        for index in 0..PAIRING_CODE_SYMBOLS {
            if index > 0 && index % GROUP_SYMBOLS == 0 {
                text.push('-');
            }
            let shift = SYMBOL_BITS * (PAIRING_CODE_SYMBOLS - 1 - index) as u32;
            text.push(char::from(
                ALPHABET[(*value >> shift & mask(SYMBOL_BITS)) as usize],
            ));
        }
        text
    }

    /// The showing computer's address, as the entering computer at `local` on a `prefix_len`
    /// subnet reads it: its own upper 16 bits with the code's lower 16. A subnet wider than /16
    /// could hide different upper bits, so it is refused rather than guessed.
    pub fn showing_address(
        &self,
        local: Ipv4Addr,
        prefix_len: u8,
    ) -> Result<Ipv4Addr, PairingCodeError> {
        if u32::from(prefix_len) < u32::BITS - ADDRESS_BITS {
            return Err(PairingCodeError::OffNetwork);
        }
        let low = (self.data >> SECRET_BITS) as u32;
        let showing = Ipv4Addr::from(u32::from(local) & !(mask(ADDRESS_BITS) as u32) | low);
        validate_subnet_peer(local, prefix_len, showing)
            .map_err(|_| PairingCodeError::OffNetwork)?;
        Ok(showing)
    }

    /// The SPAKE2 password: a fixed prefix and the 55 data bits, so the address bits are bound too.
    pub(crate) fn password(&self) -> Zeroizing<[u8; PASSWORD_BYTES]> {
        let mut password = Zeroizing::new([0; PASSWORD_BYTES]);
        password[..PASSWORD_PREFIX.len()].copy_from_slice(PASSWORD_PREFIX);
        let data = Zeroizing::new(self.data.to_be_bytes());
        password[PASSWORD_PREFIX.len()..].copy_from_slice(&data[8 - DATA_BYTES..]);
        password
    }
}

impl Drop for PairingCode {
    fn drop(&mut self) {
        self.data.zeroize();
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

fn mask(bits: u32) -> u64 {
    (1 << bits) - 1
}

fn symbol_value(byte: u8) -> Option<u64> {
    match byte {
        b'O' => Some(0),
        b'I' | b'L' => Some(1),
        _ => ALPHABET
            .iter()
            .position(|symbol| *symbol == byte)
            .map(|value| value as u64),
    }
}

/// The CRC-5 of the 55 data bits: the remainder of data * x^5 divided by the generator.
fn check(data: u64) -> u64 {
    let mut remainder = data << CHECK_BITS;
    for bit in (CHECK_BITS..CHECK_BITS + DATA_BITS).rev() {
        if remainder >> bit & 1 == 1 {
            remainder ^= CHECK_GENERATOR << (bit - CHECK_BITS);
        }
    }
    remainder
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOWING: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 30);

    fn codes() -> Vec<PairingCode> {
        let mut codes: Vec<_> = (0..48)
            .map(|_| PairingCode::generate(SHOWING).unwrap())
            .collect();
        for data in [0, 1, mask(DATA_BITS), 0x55_5555_5555_5555 & mask(DATA_BITS)] {
            codes.push(PairingCode { data });
        }
        codes
    }

    #[test]
    fn a_code_round_trips_and_carries_the_low_address_bits() {
        for code in codes() {
            let shown = code.display();
            assert_eq!(shown.len(), 14);
            assert_eq!(shown.as_bytes()[4], b'-');
            assert_eq!(shown.as_bytes()[9], b'-');
            assert_eq!(PairingCode::parse(&shown).unwrap().data, code.data);
        }
        let code = PairingCode::generate(SHOWING).unwrap();
        assert_eq!(
            code.showing_address(Ipv4Addr::new(192, 168, 1, 20), 24),
            Ok(SHOWING)
        );
        let other = PairingCode::generate(SHOWING).unwrap();
        assert_ne!(code.data, other.data, "each code has a fresh secret");
    }

    #[test]
    fn every_single_symbol_substitution_is_caught() {
        for code in codes() {
            let shown = code.display().replace('-', "");
            for position in 0..PAIRING_CODE_SYMBOLS {
                for symbol in ALPHABET
                    .iter()
                    .filter(|s| **s != shown.as_bytes()[position])
                {
                    let mut typo = shown.clone().into_bytes();
                    typo[position] = *symbol;
                    assert_eq!(
                        PairingCode::parse(std::str::from_utf8(&typo).unwrap()).unwrap_err(),
                        PairingCodeError::Mistyped
                    );
                }
            }
        }
    }

    #[test]
    fn input_is_normalized_before_checking() {
        let code = PairingCode {
            data: 0x10_1010_1010,
        };
        let shown = code.display().to_string();
        assert!(shown.contains('0') && shown.contains('1'));
        for typed in [
            shown.to_ascii_lowercase(),
            shown.replace('-', ""),
            shown.replace('-', " "),
            format!(" \t{}\n", shown.replace('-', "  ")),
            shown
                .replace('0', "O")
                .replacen('1', "I", 1)
                .replace('1', "l"),
            shown.replace('0', "o").to_ascii_lowercase(),
        ] {
            assert_eq!(
                PairingCode::parse(&typed).unwrap().data,
                code.data,
                "{typed}"
            );
        }
    }

    #[test]
    fn malformed_and_oversized_input_is_refused_before_checking() {
        let shown = PairingCode::generate(SHOWING)
            .unwrap()
            .display()
            .replace('-', "");
        let padded = format!(
            "{shown}{}",
            " ".repeat(MAX_PAIRING_CODE_INPUT_BYTES + 1 - 12)
        );
        assert_eq!(padded.len(), MAX_PAIRING_CODE_INPUT_BYTES + 1);
        let at_limit = format!("{shown}{}", " ".repeat(MAX_PAIRING_CODE_INPUT_BYTES - 12));
        assert!(PairingCode::parse(&at_limit).is_ok());
        for input in [
            String::new(),
            shown[..11].to_owned(),
            format!("{shown}0"),
            format!("{}U", &shown[..11]),
            format!("{}é", &shown[..11]),
            format!("{}_", &shown[..11]),
            padded,
        ] {
            assert_eq!(
                PairingCode::parse(&input).unwrap_err(),
                PairingCodeError::Incomplete,
                "{input}"
            );
        }
    }

    #[test]
    fn the_address_must_be_another_host_of_the_entering_subnet() {
        let code = PairingCode::generate(SHOWING).unwrap();
        for (local, prefix, expected) in [
            (Ipv4Addr::new(192, 168, 1, 20), 24, Ok(SHOWING)),
            (
                Ipv4Addr::new(192, 168, 7, 20),
                16,
                Ok(Ipv4Addr::new(192, 168, 1, 30)),
            ),
            (
                Ipv4Addr::new(192, 168, 7, 20),
                24,
                Err(PairingCodeError::OffNetwork),
            ),
            (
                Ipv4Addr::new(192, 168, 1, 30),
                24,
                Err(PairingCodeError::OffNetwork),
            ),
            (
                Ipv4Addr::new(192, 168, 1, 20),
                32,
                Err(PairingCodeError::OffNetwork),
            ),
            (
                Ipv4Addr::new(8, 8, 1, 20),
                24,
                Err(PairingCodeError::OffNetwork),
            ),
            (
                Ipv4Addr::new(192, 168, 7, 20),
                15,
                Err(PairingCodeError::OffNetwork),
            ),
        ] {
            assert_eq!(code.showing_address(local, prefix), expected);
        }
        let broadcast = PairingCode::generate(Ipv4Addr::new(192, 168, 1, 255)).unwrap();
        assert_eq!(
            broadcast.showing_address(Ipv4Addr::new(192, 168, 1, 20), 24),
            Err(PairingCodeError::OffNetwork)
        );
        let link_local = PairingCode::generate(Ipv4Addr::new(169, 254, 9, 8)).unwrap();
        assert_eq!(
            link_local.showing_address(Ipv4Addr::new(169, 254, 3, 4), 16),
            Ok(Ipv4Addr::new(169, 254, 9, 8))
        );
    }

    #[test]
    fn the_password_binds_every_data_bit_and_nothing_is_printed() {
        let code = PairingCode::generate(SHOWING).unwrap();
        let password = code.password();
        assert!(password.starts_with(PASSWORD_PREFIX));
        for bit in 0..DATA_BITS {
            let flipped = PairingCode {
                data: code.data ^ 1 << bit,
            };
            assert_ne!(*flipped.password(), *password);
        }
        assert_eq!(format!("{code:?}"), "PairingCode(<redacted>)");
    }
}

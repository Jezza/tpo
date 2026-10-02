//! IPv4/IPv6 prefixes as used in topology files.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

fn split_prefix(input: &str, max: u8) -> Result<(&str, u8), String> {
    let (addr, len) = input
        .split_once('/')
        .ok_or_else(|| format!("`{input}` must be a prefix like `10.77.0.0/24`"))?;
    let len: u8 = len
        .parse()
        .ok()
        .filter(|l| *l <= max)
        .ok_or_else(|| format!("`{input}` has an invalid prefix length"))?;
    Ok((addr, len))
}

/// An IPv4 prefix whose host bits are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn host_bits(&self) -> u32 {
        32 - u32::from(self.prefix)
    }

    fn size(&self) -> u64 {
        1u64 << self.host_bits()
    }

    /// The address with host part `index`, if it fits in the prefix.
    pub fn host(&self, index: u32) -> Option<Ipv4Addr> {
        (u64::from(index) < self.size()).then(|| Ipv4Addr::from(u32::from(self.addr) | index))
    }

    /// The host part of `addr`, if it lies inside the prefix.
    pub fn index_of(&self, addr: Ipv4Addr) -> Option<u32> {
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << self.host_bits()
        };
        (u32::from(addr) & mask == u32::from(self.addr)).then(|| u32::from(addr) & !mask)
    }

    /// The highest host index below the broadcast address.
    pub fn last_usable_index(&self) -> u32 {
        (self.size() - 2) as u32
    }
}

impl FromStr for Ipv4Net {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        let (addr, prefix) = split_prefix(input, 32)?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|_| format!("`{input}` is not an IPv4 prefix"))?;
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        };
        let base = Ipv4Addr::from(u32::from(addr) & mask);
        if base != addr {
            return Err(format!(
                "`{input}` has host bits set; did you mean `{base}/{prefix}`?"
            ));
        }
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// An IPv6 prefix whose host bits are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv6Net {
    addr: Ipv6Addr,
    prefix: u8,
}

impl Ipv6Net {
    pub fn addr(&self) -> Ipv6Addr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn mask(prefix: u8) -> u128 {
        if prefix == 0 {
            0
        } else {
            u128::MAX << (128 - u32::from(prefix))
        }
    }

    pub fn host(&self, index: u32) -> Option<Ipv6Addr> {
        let host_bits = 128 - u32::from(self.prefix);
        (host_bits >= 32 || u128::from(index) < (1u128 << host_bits))
            .then(|| Ipv6Addr::from(u128::from(self.addr) | u128::from(index)))
    }

    pub fn contains(&self, addr: Ipv6Addr) -> bool {
        u128::from(addr) & Self::mask(self.prefix) == u128::from(self.addr)
    }
}

impl FromStr for Ipv6Net {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        let (addr, prefix) = split_prefix(input, 128)?;
        let addr: Ipv6Addr = addr
            .parse()
            .map_err(|_| format!("`{input}` is not an IPv6 prefix"))?;
        let base = Ipv6Addr::from(u128::from(addr) & Self::mask(prefix));
        if base != addr {
            return Err(format!(
                "`{input}` has host bits set; did you mean `{base}/{prefix}`?"
            ));
        }
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for Ipv6Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

macro_rules! serde_via_string {
    ($ty:ty) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

serde_via_string!(Ipv4Net);
serde_via_string!(Ipv6Net);

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn parses_and_displays_ipv4_prefixes() {
        let n: Ipv4Net = "10.77.0.0/24".parse().unwrap();
        assert_eq!(n.addr(), Ipv4Addr::new(10, 77, 0, 0));
        assert_eq!(n.prefix(), 24);
        assert_eq!(n.to_string(), "10.77.0.0/24");
    }

    #[test]
    fn rejects_ipv4_prefixes_with_host_bits_or_bad_lengths() {
        assert!(
            "10.77.0.1/24"
                .parse::<Ipv4Net>()
                .unwrap_err()
                .contains("10.77.0.0/24")
        );
        assert!("10.77.0.0/33".parse::<Ipv4Net>().is_err());
        assert!("10.77.0.0".parse::<Ipv4Net>().is_err());
        assert!("nope/24".parse::<Ipv4Net>().is_err());
    }

    #[test]
    fn ipv4_host_index_round_trips() {
        let n: Ipv4Net = "10.77.0.0/24".parse().unwrap();
        assert_eq!(n.host(5), Some(Ipv4Addr::new(10, 77, 0, 5)));
        assert_eq!(n.index_of(Ipv4Addr::new(10, 77, 0, 5)), Some(5));
        assert_eq!(n.index_of(Ipv4Addr::new(10, 78, 0, 5)), None);
        assert_eq!(n.host(256), None);
        assert_eq!(n.last_usable_index(), 254);
        let wide: Ipv4Net = "10.0.0.0/16".parse().unwrap();
        assert_eq!(wide.last_usable_index(), 65534);
        assert_eq!(wide.host(300), Some(Ipv4Addr::new(10, 0, 1, 44)));
    }

    #[test]
    fn parses_ipv6_prefixes_and_hosts() {
        let n: Ipv6Net = "fd77::/64".parse().unwrap();
        assert_eq!(n.to_string(), "fd77::/64");
        assert_eq!(n.host(5), Some("fd77::5".parse::<Ipv6Addr>().unwrap()));
        assert_eq!(n.host(300), Some("fd77::12c".parse::<Ipv6Addr>().unwrap()));
        assert!("fd77::1/64".parse::<Ipv6Net>().is_err());
        assert!("fd77::/129".parse::<Ipv6Net>().is_err());
        assert!(n.contains("fd77::9".parse().unwrap()));
        assert!(!n.contains("fd78::9".parse().unwrap()));
    }
}

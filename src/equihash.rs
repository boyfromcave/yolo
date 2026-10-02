// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Equihash parameters: only n, k and the solution length matter to a stratum server.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Equihash {
    pub n: u32,
    pub k: u32,
}

impl Equihash {
    pub const REGTEST: Equihash = Equihash { n: 48, k: 5 };
    pub const MAINNET: Equihash = Equihash { n: 192, k: 7 };

    /// For `chain` as `getblockchaininfo` reports it: regtest → 48,5, anything else → 192,7
    /// (Ycash mainnet and testnet both use 192,7; `ref/ycash/src/chainparams.cpp:560-563`
    /// gives regtest's 48,5).
    pub fn for_chain(chain: &str) -> Equihash {
        if chain == "regtest" {
            Equihash::REGTEST
        } else {
            Equihash::MAINNET
        }
    }

    /// Bytes of a solution: `2^k * (n/(k+1) + 1) / 8` (36 at 48,5; 400 at 192,7; 1344 at 200,9).
    pub fn solution_len(&self) -> usize {
        ((1usize << self.k) * (self.n as usize / (self.k as usize + 1) + 1)) / 8
    }

    /// Bytes the miner sends in `mining.submit` params[4]: compact size + solution.
    pub fn solution_wire_len(&self) -> usize {
        crate::codec::compact_size(self.solution_len() as u64).len() + self.solution_len()
    }

    /// Checks the hex the miner sent: right length and a compact-size prefix matching it.
    pub fn check_solution_hex(&self, hex_solution: &str) -> Result<Vec<u8>, String> {
        let bytes = hex::decode(hex_solution).map_err(|e| format!("solution is not hex: {}", e))?;
        let expected_prefix = crate::codec::compact_size(self.solution_len() as u64);
        if bytes.len() != self.solution_wire_len() || bytes[..expected_prefix.len()] != expected_prefix[..] {
            return Err(format!(
                "solution is {} bytes, expected {} ({} + {} for {},{})",
                bytes.len(),
                self.solution_wire_len(),
                hex::encode(&expected_prefix),
                self.solution_len(),
                self.n,
                self.k
            ));
        }
        Ok(bytes)
    }
}

impl fmt::Display for Equihash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.n, self.k)
    }
}

/// `auto`, `48,5`, `192,7` (or `48/5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquihashArg {
    Auto,
    Fixed(Equihash),
}

impl FromStr for EquihashArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(EquihashArg::Auto);
        }
        let (n, k) = s.split_once([',', '/']).ok_or_else(|| format!("expected auto or N,K, got {:?}", s))?;
        let n: u32 = n.trim().parse().map_err(|_| format!("bad n in {:?}", s))?;
        let k: u32 = k.trim().parse().map_err(|_| format!("bad k in {:?}", s))?;
        if k == 0 || k >= n || n % (k + 1) != 0 {
            return Err(format!("{},{} is not a valid Equihash parameter pair", n, k));
        }
        Ok(EquihashArg::Fixed(Equihash { n, k }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solution_lengths() {
        assert_eq!(Equihash::REGTEST.solution_len(), 36);
        assert_eq!(Equihash::REGTEST.solution_wire_len(), 37);
        assert_eq!(Equihash::MAINNET.solution_len(), 400);
        assert_eq!(Equihash::MAINNET.solution_wire_len(), 403);
        assert_eq!(Equihash { n: 200, k: 9 }.solution_len(), 1344);
    }

    #[test]
    fn checks_wire_solution() {
        let sol = format!("24{}", "ab".repeat(36));
        assert_eq!(Equihash::REGTEST.check_solution_hex(&sol).unwrap().len(), 37);
        assert!(Equihash::REGTEST.check_solution_hex(&format!("24{}", "ab".repeat(35))).is_err());
        assert!(Equihash::REGTEST.check_solution_hex(&format!("23{}", "ab".repeat(36))).is_err());
        assert!(Equihash::REGTEST.check_solution_hex("zz").is_err());
        let big = format!("fd9001{}", "cd".repeat(400));
        assert_eq!(Equihash::MAINNET.check_solution_hex(&big).unwrap().len(), 403);
        assert!(Equihash::MAINNET.check_solution_hex(&sol).is_err());
        // the regtest block 105 solution, as the miner would send it
        let raw = include_str!("../tests/vectors/regtest-block-105.hex").trim();
        assert!(Equihash::REGTEST.check_solution_hex(&raw[280..280 + 74]).is_ok());
    }

    #[test]
    fn parses_argument() {
        assert_eq!("auto".parse::<EquihashArg>().unwrap(), EquihashArg::Auto);
        assert_eq!("48,5".parse::<EquihashArg>().unwrap(), EquihashArg::Fixed(Equihash::REGTEST));
        assert_eq!("192/7".parse::<EquihashArg>().unwrap(), EquihashArg::Fixed(Equihash::MAINNET));
        assert!("50,5".parse::<EquihashArg>().is_err());
        assert!("5,5".parse::<EquihashArg>().is_err());
        assert!("x".parse::<EquihashArg>().is_err());
        assert_eq!(Equihash::for_chain("regtest"), Equihash::REGTEST);
        assert_eq!(Equihash::for_chain("main"), Equihash::MAINNET);
    }
}

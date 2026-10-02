// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Byte helpers that mirror the Perl (`ref/yolo/stratumsolo`): `reverse_bytes`, `int_to_hex`,
//! `compact_size`, `hash_this`, `merkleroot`. Everything here works on bytes; hex is only at
//! the edges.

use sha2::{Digest, Sha256};

/// `reverse_bytes` on a hex string (`stratumsolo:327`): reverses byte order, keeps each byte.
pub fn reverse_hex(hex: &str) -> String {
    let bytes = hex.as_bytes();
    let mut out = String::with_capacity(hex.len());
    let mut i = bytes.len();
    while i >= 2 {
        out.push_str(&hex[i - 2..i]);
        i -= 2;
    }
    out
}

/// `int_to_hex(n, 32, 'r')`: a u32 as 8 hex chars, little-endian.
pub fn u32_le_hex(n: u32) -> String {
    hex::encode(n.to_le_bytes())
}

/// Bitcoin compact size (`stratumsolo:337`), corrected: the Perl skips 65536..=65556.
pub fn compact_size(n: u64) -> Vec<u8> {
    if n < 253 {
        vec![n as u8]
    } else if n <= 0xffff {
        let mut v = vec![0xfd];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= 0xffff_ffff {
        let mut v = vec![0xfe];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![0xff];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
}

/// Reads a compact size at `pos`; returns (value, bytes consumed).
pub fn read_compact_size(data: &[u8], pos: usize) -> Result<(u64, usize), String> {
    let first = *data.get(pos).ok_or("compact size: out of data")?;
    let take = |n: usize| -> Result<&[u8], String> {
        data.get(pos + 1..pos + 1 + n).ok_or_else(|| "compact size: truncated".to_string())
    };
    Ok(match first {
        0..=252 => (first as u64, 1),
        0xfd => (u16::from_le_bytes(take(2)?.try_into().unwrap()) as u64, 3),
        0xfe => (u32::from_le_bytes(take(4)?.try_into().unwrap()) as u64, 5),
        _ => (u64::from_le_bytes(take(8)?.try_into().unwrap()), 9),
    })
}

/// SHA256d, internal byte order (what goes into a header or a merkle node).
pub fn dsha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    second.into()
}

/// A txid as the node displays it (`hash_this(..., 'le')` in the Perl): SHA256d, byte-reversed.
pub fn txid_display(raw_tx: &[u8]) -> String {
    let mut h = dsha256(raw_tx);
    h.reverse();
    hex::encode(h)
}

/// Display hex (big-endian, as RPC prints hashes) → internal 32-byte order.
pub fn hash_from_display(hex_str: &str) -> Result<[u8; 32], String> {
    let mut bytes: [u8; 32] = hex::decode(hex_str)
        .map_err(|e| format!("bad hash hex: {}", e))?
        .try_into()
        .map_err(|_| "hash is not 32 bytes".to_string())?;
    bytes.reverse();
    Ok(bytes)
}

/// Merkle root over txids given in internal byte order (`merkleroot`, `stratumsolo:370`):
/// pairs are concatenated and SHA256d, the last node is duplicated on odd levels, one txid
/// is its own root. Result is in internal order (what the header carries).
pub fn merkle_root(txids: &[[u8; 32]]) -> [u8; 32] {
    assert!(!txids.is_empty(), "merkle root of no transactions");
    let mut level: Vec<[u8; 32]> = txids.to_vec();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            let last = *level.last().unwrap();
            level.push(last);
        }
        level = level
            .chunks(2)
            .map(|pair| {
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&pair[0]);
                buf[32..].copy_from_slice(&pair[1]);
                dsha256(&buf)
            })
            .collect();
    }
    level[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_hex_reverses_bytes_not_nibbles() {
        assert_eq!(reverse_hex("0102ab"), "ab0201");
        assert_eq!(reverse_hex(""), "");
        assert_eq!(reverse_hex("200f0f0f"), "0f0f0f20");
    }

    #[test]
    fn u32_le_hex_matches_int_to_hex_reversed() {
        // int_to_hex(4, 32, 'r') in the Perl → "04000000"
        assert_eq!(u32_le_hex(4), "04000000");
        assert_eq!(u32_le_hex(1_790_581_942), "b61cba6a");
    }

    #[test]
    fn compact_size_boundaries() {
        assert_eq!(compact_size(0), vec![0x00]);
        assert_eq!(compact_size(3), vec![0x03]);
        assert_eq!(compact_size(36), vec![0x24]);
        assert_eq!(compact_size(252), vec![0xfc]);
        assert_eq!(compact_size(253), vec![0xfd, 0xfd, 0x00]);
        assert_eq!(compact_size(400), vec![0xfd, 0x90, 0x01]);
        assert_eq!(compact_size(0xffff), vec![0xfd, 0xff, 0xff]);
        assert_eq!(compact_size(0x10000), vec![0xfe, 0x00, 0x00, 0x01, 0x00]);
        assert_eq!(compact_size(0x1_0000_0000), vec![0xff, 0, 0, 0, 0, 1, 0, 0, 0]);
        for n in [0u64, 1, 252, 253, 400, 0xffff, 0x10000, 0xffff_ffff, 0x1_0000_0000] {
            let enc = compact_size(n);
            assert_eq!(read_compact_size(&enc, 0).unwrap(), (n, enc.len()));
        }
        assert!(read_compact_size(&[0xfd, 0x01], 0).is_err());
    }

    #[test]
    fn coinbase_txid_matches_template_hash() {
        let t: serde_json::Value =
            serde_json::from_str(include_str!("../tests/vectors/regtest-template-105.json")).unwrap();
        let data = hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(txid_display(&data), t["coinbasetxn"]["hash"].as_str().unwrap());
    }

    #[test]
    fn merkle_root_of_regtest_block_105() {
        // Block 105 (regtest, three transactions): txids from `getblock 105 1`, root from the raw
        // header bytes 68..100 of `getblock <hash> 0`.
        let txids = [
            "0adc95af17af6513757bd5cd229fc5fef6b436aa02cac5d36b4bfecbaf63d4f7",
            "a80555562c7669ae53157f380e77e40a0a60e6b5066c3c84122ecf3d3f44f331",
            "fb9d25619e7c33e39b6201184641cb411bda65bba9ea05e4da114278dd5fbe40",
        ]
        .iter()
        .map(|h| hash_from_display(h).unwrap())
        .collect::<Vec<_>>();
        let raw = hex::decode(include_str!("../tests/vectors/regtest-block-105.hex").trim()).unwrap();
        assert_eq!(&merkle_root(&txids)[..], &raw[36..68]);
        // one transaction: the root is the txid
        assert_eq!(merkle_root(&txids[..1]), txids[0]);
        // two: no duplication
        let two = merkle_root(&txids[..2]);
        let mut buf = Vec::new();
        buf.extend_from_slice(&txids[0]);
        buf.extend_from_slice(&txids[1]);
        assert_eq!(two, dsha256(&buf));
    }
}

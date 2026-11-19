// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! The Yellowback coinbase tag, decoded the way the node does it
//! (`ycash-dd/contrib/yellowback/yellowback_price.py` `decode_coinbase_tag`, mirroring
//! `src/yellowback/tag.cpp`): skip the BIP34 height push, scan the rest of the scriptSig for the
//! first occurrence of either 5-byte pattern, `24 59 45 44 21` (v1: a 36-byte push followed by the
//! magic `YED!`) or `31 59 45 44 21` (v2, beta4: a 49-byte push), and decode the body that follows.
//! The first occurrence decides (TAG-5): a v1 pattern must be followed by version 1, a v2 pattern by
//! version 2; anything else, or an invalid body, means "no tag".
//!
//! v1 (36 bytes): `YED!` | 0x01 | flags | priceMicroUsd u64 | sourceMask u16 | payoutKey 20 (Hash160).
//! v2 (49 bytes, rpcversion 11, workspace `docs/plans/beta4/price-feed.md` §4.3): `YED!` | 0x02 |
//! flags | priceMicroUsd u64 | sourceMask u16 | payoutScheme u8 | payoutHash 32, where payoutScheme
//! 0x01 = SLH-DSA-SHA2-128s and 0x02 = FN-DSA-512 (the post-quantum key id SHA256(scheme ‖ pk)).
//!
//! yolo never builds or changes a tag (it copies `coinbaseaux.flags` verbatim); it decodes one only
//! for its log lines and `/status`.

pub const TAG_PATTERN: [u8; 5] = [0x24, b'Y', b'E', b'D', b'!'];
pub const TAG_V2_PATTERN: [u8; 5] = [0x31, b'Y', b'E', b'D', b'!'];
pub const TAG_SIZE: usize = 36;
pub const TAG_V2_SIZE: usize = 49;
pub const TAG_PUSH_SIZE: usize = TAG_SIZE + 1;
pub const TAG_V2_PUSH_SIZE: usize = TAG_V2_SIZE + 1;
pub const TAG_VERSION: u8 = 1;
pub const TAG_V2_VERSION: u8 = 2;
/// payoutScheme of a v2 tag: SLH-DSA-SHA2-128s.
pub const SCHEME_SLH_DSA: u8 = 0x01;
/// payoutScheme of a v2 tag: FN-DSA-512 (Falcon).
pub const SCHEME_FN_DSA: u8 = 0x02;
pub const PRICE_MIN: i64 = 100;
/// The node's PRICE_MAX (ycash6 / ycash-dd `src/yellowback/params.h`: 4,294,967,295 µUSD, about
/// $4,295). It was $100 here, so a valid quote above $100 read as "no tag" in yolo's logs and
/// /status; the node and the chain were never affected (yolo copies the flags verbatim).
pub const PRICE_MAX: i64 = 4_294_967_295;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub version: u8,
    pub signal: bool,
    pub price_micro_usd: i64,
    pub source_mask: u16,
    /// The payee kind: 0 = P2PKH (v1), else the v2 tag's payoutScheme (1 SLH-DSA, 2 FN-DSA).
    pub payout_scheme: u8,
    /// The payee hash: the 20-byte Hash160 of a v1 tag, the 32-byte post-quantum key hash of a v2 tag.
    pub payout_hash: Vec<u8>,
    /// Byte offset of the push opcode (`0x24` or `0x31`) within the scriptSig.
    pub offset: usize,
}

impl Tag {
    /// `quote` when a price is carried, `signal` for a price of 0.
    pub fn kind(&self) -> &'static str {
        if self.price_micro_usd > 0 {
            "quote"
        } else {
            "signal"
        }
    }

    /// The payee's scheme as `yed_gettag` names it (`payoutScheme`).
    pub fn payout_scheme_name(&self) -> &'static str {
        scheme_name(self.payout_scheme)
    }

    /// The fee-output script of the payee (beta4 P1 spec §2): `76 a9 14 <20> 88 ac` for P2PKH,
    /// `20 <32> OP_1|OP_2 OP_CHECKPQSIG` (0xc2) for a post-quantum key.
    pub fn payee_script(&self) -> Vec<u8> {
        if self.payout_scheme == 0 {
            let mut s = vec![0x76, 0xa9, 0x14];
            s.extend_from_slice(&self.payout_hash);
            s.extend([0x88, 0xac]);
            s
        } else {
            let mut s = vec![0x20];
            s.extend_from_slice(&self.payout_hash);
            s.extend([0x50 + self.payout_scheme, 0xc2]);
            s
        }
    }
}

/// `p2pkh`, `slh-dsa-sha2-128s` or `fn-dsa-512` (the names `yed_gettag.payoutScheme` uses).
pub fn scheme_name(kind: u8) -> &'static str {
    match kind {
        0 => "p2pkh",
        SCHEME_SLH_DSA => "slh-dsa-sha2-128s",
        SCHEME_FN_DSA => "fn-dsa-512",
        _ => "unknown",
    }
}

/// Length of the height push at the start of a coinbase scriptSig (`CScript() << nHeight`),
/// or None when the script does not begin with a well-formed push.
pub fn height_push_len(script: &[u8]) -> Option<usize> {
    let op = *script.first()?;
    let (n, hdr) = match op {
        0x00 | 0x4f | 0x51..=0x60 => return Some(1),
        0x01..=0x4b => (op as usize, 1),
        0x4c => (*script.get(1)? as usize, 2),
        0x4d => (u16::from_le_bytes([*script.get(1)?, *script.get(2)?]) as usize, 3),
        0x4e => (u32::from_le_bytes([*script.get(1)?, *script.get(2)?, *script.get(3)?, *script.get(4)?]) as usize, 5),
        _ => return None,
    };
    if script.len() >= hdr + n {
        Some(hdr + n)
    } else {
        None
    }
}

/// `CScript() << nHeight` for the heights a coinbase can carry (BIP34 minimal push).
pub fn height_push(height: u32) -> Vec<u8> {
    if height == 0 {
        return vec![0x00];
    }
    if (1..=16).contains(&height) {
        return vec![0x50 + height as u8];
    }
    let mut n = height;
    let mut out = Vec::new();
    while n > 0 {
        out.push((n & 0xff) as u8);
        n >>= 8;
    }
    if out.last().is_some_and(|b| b & 0x80 != 0) {
        out.push(0);
    }
    let mut v = vec![out.len() as u8];
    v.extend(out);
    v
}

fn price_ok(price: i64) -> bool {
    price == 0 || (PRICE_MIN..=PRICE_MAX).contains(&price)
}

/// The body after a v1 pattern: version 1, the reserved flag bits clear, a valid price.
fn decode_v1(body: &[u8]) -> Option<(u8, i64, u16, u8, Vec<u8>)> {
    if body.len() < TAG_SIZE - 4 || body[0] != TAG_VERSION || body[1] & 0xfe != 0 {
        return None;
    }
    let price = i64::from_le_bytes(body[2..10].try_into().ok()?);
    let mask = u16::from_le_bytes(body[10..12].try_into().ok()?);
    price_ok(price).then(|| (body[1], price, mask, 0, body[12..32].to_vec()))
}

/// The body after a v2 pattern: version 2, the reserved flag bits clear, a valid price, a known
/// payoutScheme, and all 45 bytes present.
fn decode_v2(body: &[u8]) -> Option<(u8, i64, u16, u8, Vec<u8>)> {
    if body.len() < TAG_V2_SIZE - 4 || body[0] != TAG_V2_VERSION || body[1] & 0xfe != 0 {
        return None;
    }
    let price = i64::from_le_bytes(body[2..10].try_into().ok()?);
    let mask = u16::from_le_bytes(body[10..12].try_into().ok()?);
    let scheme = body[12];
    if scheme != SCHEME_SLH_DSA && scheme != SCHEME_FN_DSA {
        return None;
    }
    price_ok(price).then(|| (body[1], price, mask, scheme, body[13..45].to_vec()))
}

/// TAG-1/TAG-5 over a raw coinbase scriptSig: the first occurrence of either pattern decides.
pub fn decode_coinbase_tag(script_sig: &[u8]) -> Option<Tag> {
    let start = height_push_len(script_sig)?;
    let rest = &script_sig[start..];
    let i = rest.windows(TAG_PATTERN.len()).position(|w| w == TAG_PATTERN || w == TAG_V2_PATTERN)?;
    let body = &rest[i + TAG_PATTERN.len()..];
    let (version, decoded) = if rest[i] == TAG_PATTERN[0] { (TAG_VERSION, decode_v1(body)) } else { (TAG_V2_VERSION, decode_v2(body)) };
    let (flags, price_micro_usd, source_mask, payout_scheme, payout_hash) = decoded?;
    Some(Tag { version, signal: flags & 1 == 1, price_micro_usd, source_mask, payout_scheme, payout_hash, offset: start + i })
}

/// `quote`, `signal` or `none` for a scriptSig, for the log line and `/status`.
pub fn tag_kind(script_sig: &[u8]) -> &'static str {
    decode_coinbase_tag(script_sig).map(|t| t.kind()).unwrap_or("none")
}

#[cfg(test)]
mod tests {
    use super::*;

    // coinbaseaux.flags of regtest template 105 (quote 123456 µUSD, mask 3, signalling).
    const FLAGS: &str = "2459454421010140e2010000000000030092323df36253f52973ac6515e62601674e2a2fe8";

    fn flags() -> Vec<u8> {
        hex::decode(FLAGS).unwrap()
    }

    #[test]
    fn height_push_forms() {
        assert_eq!(height_push(0), vec![0x00]);
        assert_eq!(height_push(4), vec![0x54]);
        assert_eq!(height_push(16), vec![0x60]);
        assert_eq!(height_push(17), vec![0x01, 0x11]);
        assert_eq!(height_push(105), vec![0x01, 0x69]);
        assert_eq!(height_push(128), vec![0x02, 0x80, 0x00]);
        assert_eq!(height_push(1_000_000), vec![0x03, 0x40, 0x42, 0x0f]);
        for h in [0u32, 1, 16, 17, 105, 127, 128, 255, 256, 32767, 32768, 65535, 65536, 1_000_000, 8_388_608] {
            assert_eq!(height_push_len(&height_push(h)), Some(height_push(h).len()), "height {}", h);
        }
        assert_eq!(height_push_len(&[]), None);
        assert_eq!(height_push_len(&[0x03, 0x01]), None);
        assert_eq!(height_push_len(&[0xff]), None);
    }

    #[test]
    fn decodes_regtest_quote_tag() {
        // scriptSig of the template-105 coinbase: 01 69 | 00 | flags
        let mut sig = vec![0x01, 0x69, 0x00];
        sig.extend(flags());
        let tag = decode_coinbase_tag(&sig).expect("tag");
        assert_eq!(tag.kind(), "quote");
        assert_eq!(tag.price_micro_usd, 123_456);
        assert_eq!(tag.source_mask, 3);
        assert!(tag.signal);
        assert_eq!(tag.offset, 3);
        assert_eq!(hex::encode(&tag.payout_hash), "92323df36253f52973ac6515e62601674e2a2fe8");
        assert_eq!(tag.payout_scheme_name(), "p2pkh");
        assert_eq!(tag.version, 1);
        assert_eq!(tag_kind(&sig), "quote");
    }

    #[test]
    fn signal_only_and_none() {
        let mut f = flags();
        f[7..15].copy_from_slice(&0i64.to_le_bytes());
        let mut sig = vec![0x54];
        sig.extend(&f);
        assert_eq!(tag_kind(&sig), "signal");
        assert_eq!(tag_kind(&[0x54, 0x00]), "none");
        assert_eq!(tag_kind(&[]), "none");
        // pattern inside the height push itself does not count
        let mut sig = vec![0x05];
        sig.extend(&TAG_PATTERN);
        sig.extend([0u8; 40]);
        assert_eq!(tag_kind(&sig), "none");
    }

    #[test]
    fn invalid_bodies_are_no_tag() {
        let mut f = flags();
        f[5] = 2; // version
        let mut sig = vec![0x54];
        sig.extend(&f);
        assert_eq!(tag_kind(&sig), "none");
        let mut f = flags();
        f[7..15].copy_from_slice(&(PRICE_MAX + 1).to_le_bytes());
        let mut sig = vec![0x54];
        sig.extend(&f);
        assert_eq!(tag_kind(&sig), "none");
        // the node's ceiling itself, and a price above the old $100 limit, are quotes
        for price in [PRICE_MAX, 250_000_000] {
            let mut f = flags();
            f[7..15].copy_from_slice(&price.to_le_bytes());
            let mut sig = vec![0x54];
            sig.extend(&f);
            assert_eq!(tag_kind(&sig), "quote", "price {}", price);
        }
        let mut f = flags();
        f[6] = 2; // flags bit 1 is reserved
        let mut sig = vec![0x54];
        sig.extend(&f);
        assert_eq!(tag_kind(&sig), "none");
        // truncated
        let mut sig = vec![0x54];
        sig.extend(&flags()[..30]);
        assert_eq!(tag_kind(&sig), "none");
    }

    #[test]
    fn first_occurrence_wins_and_extranonce_bytes_are_harmless() {
        let mut sig = vec![0x01, 0x69, 0x08, 1, 2, 3, 4, 5, 6, 7, 8];
        sig.extend(flags());
        sig.extend([0x04, b'p', b'o', b'o', b'l']);
        sig.extend(flags());
        let tag = decode_coinbase_tag(&sig).unwrap();
        assert_eq!(tag.offset, 11);
    }

    /// The shared tag vectors (ycash-dd `src/test/data/yellowback_tag_vectors.json`, beta4 P1 D-T4;
    /// ycash6 and kogany carry the same file byte for byte): v1, v2 by scheme, the no-tag cases and
    /// the first-occurrence rule, each with the payee's fee-output script.
    #[test]
    fn shared_tag_vectors() {
        let doc: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/yellowback_tag_vectors.json")).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        assert!(cases.len() >= 9, "{} cases", cases.len());
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let sig = hex::decode(c["scriptSig"].as_str().unwrap()).unwrap();
            assert_eq!(sig.len() as u64, c["scriptSigSize"].as_u64().unwrap(), "{}", name);
            match (decode_coinbase_tag(&sig), &c["tag"]) {
                (None, serde_json::Value::Null) => assert_eq!(tag_kind(&sig), "none", "{}", name),
                (Some(t), want) if want.is_object() => {
                    assert_eq!(t.version as u64, want["version"].as_u64().unwrap(), "{}", name);
                    assert_eq!(t.signal, want["flags"].as_u64().unwrap() & 1 == 1, "{}", name);
                    assert_eq!(t.price_micro_usd as u64, want["priceMicroUsd"].as_u64().unwrap(), "{}", name);
                    assert_eq!(t.source_mask as u64, want["sourceMask"].as_u64().unwrap(), "{}", name);
                    assert_eq!(t.payout_scheme as u64, want["payeeKind"].as_u64().unwrap(), "{}", name);
                    assert_eq!(t.payout_scheme_name(), want["payoutScheme"].as_str().unwrap(), "{}", name);
                    assert_eq!(hex::encode(&t.payout_hash), want["payoutHash"].as_str().unwrap(), "{}", name);
                    assert_eq!(hex::encode(t.payee_script()), c["payeeScript"].as_str().unwrap(), "{}", name);
                }
                (got, want) => panic!("{}: decoded {:?}, expected {}", name, got, want),
            }
        }
    }

    #[test]
    fn v2_survives_the_rebuild_budget() {
        // a v2 tag is a 50-byte push: height push (4) + flags (50) + a 46-byte text push = 100
        let doc: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/yellowback_tag_vectors.json")).unwrap();
        let v2 = hex::decode(doc["cases"][1]["scriptSig"].as_str().unwrap()).unwrap();
        let at = decode_coinbase_tag(&v2).unwrap().offset;
        let flags = &v2[at..at + TAG_V2_PUSH_SIZE];
        assert_eq!(at + TAG_V2_PUSH_SIZE, v2.len());
        let r = crate::tx::rebuild_script_sig(&height_push(8_388_607), flags, &[b'y'; 200]).unwrap();
        assert_eq!(r.script_sig.len(), 100);
        let t = decode_coinbase_tag(&r.script_sig).expect("v2 tag survives");
        assert_eq!((t.version, t.payout_scheme_name()), (2, "fn-dsa-512"));
    }
}

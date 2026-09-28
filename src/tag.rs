//! The Yellowback coinbase tag, decoded the way the node does it
//! (`ycash-dd/contrib/yellowback/yellowback_price.py` `decode_coinbase_tag`, mirroring
//! `src/yellowback/tag.cpp`): skip the BIP34 height push, scan the rest of the scriptSig for
//! the 5-byte pattern `24 59 45 44 21` (a 36-byte push followed by the magic `YED!`), decode
//! the 32 bytes that follow. The first occurrence wins; an invalid body means "no tag".

pub const TAG_PATTERN: [u8; 5] = [0x24, b'Y', b'E', b'D', b'!'];
pub const TAG_SIZE: usize = 36;
pub const TAG_PUSH_SIZE: usize = TAG_SIZE + 1;
pub const TAG_VERSION: u8 = 1;
pub const PRICE_MIN: i64 = 100;
pub const PRICE_MAX: i64 = 100_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub version: u8,
    pub signal: bool,
    pub price_micro_usd: i64,
    pub source_mask: u16,
    pub payout_key: [u8; 20],
    /// Byte offset of the `0x24` push opcode within the scriptSig.
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
        0x4e => (
            u32::from_le_bytes([*script.get(1)?, *script.get(2)?, *script.get(3)?, *script.get(4)?]) as usize,
            5,
        ),
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
    if out.last().unwrap() & 0x80 != 0 {
        out.push(0);
    }
    let mut v = vec![out.len() as u8];
    v.extend(out);
    v
}

fn decode_body(body: &[u8]) -> Option<(u8, u8, i64, u16, [u8; 20])> {
    if body.len() < TAG_SIZE - 4 {
        return None;
    }
    let version = body[0];
    let flags = body[1];
    let price = i64::from_le_bytes(body[2..10].try_into().unwrap());
    let mask = u16::from_le_bytes(body[10..12].try_into().unwrap());
    let payout: [u8; 20] = body[12..32].try_into().unwrap();
    if version != TAG_VERSION || flags & 0xfe != 0 {
        return None;
    }
    if price != 0 && !(PRICE_MIN..=PRICE_MAX).contains(&price) {
        return None;
    }
    Some((version, flags, price, mask, payout))
}

/// TAG-1/TAG-5 over a raw coinbase scriptSig.
pub fn decode_coinbase_tag(script_sig: &[u8]) -> Option<Tag> {
    let start = height_push_len(script_sig)?;
    let rest = &script_sig[start..];
    let i = rest.windows(TAG_PATTERN.len()).position(|w| w == TAG_PATTERN)?;
    let (version, flags, price_micro_usd, source_mask, payout_key) =
        decode_body(&rest[i + TAG_PATTERN.len()..])?;
    Some(Tag {
        version,
        signal: flags & 1 == 1,
        price_micro_usd,
        source_mask,
        payout_key,
        offset: start + i,
    })
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
        assert_eq!(hex::encode(tag.payout_key), "92323df36253f52973ac6515e62601674e2a2fe8");
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
}

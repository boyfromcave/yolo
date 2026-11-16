// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! A minimal Ycash v4 (Sapling) transaction codec, sufficient for a coinbase: header
//! (`nVersion | fOverwintered`, `nVersionGroupId`), one input (null prevout, scriptSig,
//! sequence), the outputs, then the tail (`nLockTime`, `nExpiryHeight`, `valueBalance` and the
//! shielded part).
//!
//! The shielded part is carried as opaque bytes. Its shape is checked (no Sapling spend and no
//! JoinSplit, as a coinbase allows; each Sapling output 948 bytes; the binding signature when
//! there is an output) so that nothing trails the transaction, but it is never parsed further
//! nor rewritten: a Sapling coinbase from `getcoinbasetemplatefor` is signed over its outputs and
//! `valueBalance` (ZIP 243), and a pool may change only its scriptSig
//! (`docs/plans/beta3/getcoinbasetemplatefor.md` §5.5).
//!
//! The Perl rewrites `coinbasetxn.data` with a regex over the hex (`ref/yolo/stratumpool:509`);
//! this parses the structure and rewrites the field.

use crate::codec::{compact_size, read_compact_size};

pub const SAPLING_TX_VERSION: u32 = 4;
pub const OVERWINTERED_FLAG: u32 = 0x8000_0000;
pub const SAPLING_VERSION_GROUP_ID: u32 = 0x892F_2085;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOut {
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coinbase {
    pub header: u32,
    pub version_group_id: u32,
    pub script_sig: Vec<u8>,
    pub sequence: u32,
    pub vout: Vec<TxOut>,
    pub lock_time: u32,
    pub expiry_height: u32,
    pub value_balance: i64,
    /// Everything after `valueBalance`, verbatim: the three shielded counts, the Sapling outputs
    /// and the binding signature. `[0, 0, 0]` for a transparent coinbase.
    pub shielded: Vec<u8>,
    /// The number of Sapling outputs in `shielded`.
    pub sapling_outputs: u64,
}

/// A Sapling output description in a v4 transaction: cv, cmu, ephemeral key (32 each), the
/// encrypted note (580), the outgoing ciphertext (80) and the Groth16 proof (192).
pub const SAPLING_OUTPUT_SIZE: usize = 32 + 32 + 32 + 580 + 80 + 192;
/// The binding signature that follows the shielded parts when there is a spend or an output.
pub const BINDING_SIG_SIZE: usize = 64;

/// `pos + len` for a node-supplied compact size, without overflow (audit H-17).
fn span(pos: usize, len: u64) -> Result<usize, String> {
    usize::try_from(len).ok().and_then(|n| pos.checked_add(n)).ok_or_else(|| format!("length {} out of range", len))
}

fn u32_at(data: &[u8], pos: usize) -> Result<u32, String> {
    let b: [u8; 4] = data.get(pos..pos + 4).and_then(|b| b.try_into().ok()).ok_or("truncated u32")?;
    Ok(u32::from_le_bytes(b))
}

fn u64_at(data: &[u8], pos: usize) -> Result<u64, String> {
    let b: [u8; 8] = data.get(pos..pos + 8).and_then(|b| b.try_into().ok()).ok_or("truncated u64")?;
    Ok(u64::from_le_bytes(b))
}

impl Coinbase {
    pub fn parse(data: &[u8]) -> Result<Coinbase, String> {
        let header = u32_at(data, 0)?;
        if header & OVERWINTERED_FLAG == 0 || header & !OVERWINTERED_FLAG != SAPLING_TX_VERSION {
            return Err(format!("not a v4 Sapling transaction (header {:08x})", header));
        }
        let version_group_id = u32_at(data, 4)?;
        if version_group_id != SAPLING_VERSION_GROUP_ID {
            return Err(format!("unexpected nVersionGroupId {:08x}", version_group_id));
        }
        let mut pos = 8;
        let (vin_count, n) = read_compact_size(data, pos)?;
        pos += n;
        if vin_count != 1 {
            return Err(format!("coinbase must have one input, has {}", vin_count));
        }
        let prevout = data.get(pos..pos + 36).ok_or("truncated prevout")?;
        if prevout[..32].iter().any(|&b| b != 0) || prevout[32..] != [0xff; 4] {
            return Err("input is not a coinbase (prevout not null)".into());
        }
        pos += 36;
        let (sig_len, n) = read_compact_size(data, pos)?;
        pos += n;
        let end = span(pos, sig_len)?;
        let script_sig = data.get(pos..end).ok_or("truncated scriptSig")?.to_vec();
        pos = end;
        let sequence = u32_at(data, pos)?;
        pos += 4;
        let (vout_count, n) = read_compact_size(data, pos)?;
        pos += n;
        let mut vout = Vec::with_capacity(vout_count as usize);
        for _ in 0..vout_count {
            let value = u64_at(data, pos)?;
            pos += 8;
            let (spk_len, n) = read_compact_size(data, pos)?;
            pos += n;
            let end = span(pos, spk_len)?;
            let script_pubkey = data.get(pos..end).ok_or("truncated scriptPubKey")?.to_vec();
            pos = end;
            vout.push(TxOut { value, script_pubkey });
        }
        let lock_time = u32_at(data, pos)?;
        let expiry_height = u32_at(data, pos + 4)?;
        let value_balance = u64_at(data, pos + 8)? as i64;
        pos += 16;
        let shielded_start = pos;
        let (spends, n) = read_compact_size(data, pos)?;
        if spends != 0 {
            return Err(format!("coinbase has {} vShieldedSpend entries", spends));
        }
        pos += n;
        let (sapling_outputs, n) = read_compact_size(data, pos)?;
        pos += n;
        let outputs_len = usize::try_from(sapling_outputs).ok().and_then(|c| c.checked_mul(SAPLING_OUTPUT_SIZE)).ok_or_else(|| format!("{} Sapling outputs out of range", sapling_outputs))?;
        pos = span(pos, outputs_len as u64)?;
        if pos > data.len() {
            return Err("truncated Sapling outputs".into());
        }
        let (joinsplits, n) = read_compact_size(data, pos)?;
        if joinsplits != 0 {
            return Err(format!("coinbase has {} vJoinSplit entries", joinsplits));
        }
        pos += n;
        if sapling_outputs > 0 {
            pos += BINDING_SIG_SIZE;
            if pos > data.len() {
                return Err("truncated binding signature".into());
            }
        }
        if pos != data.len() {
            return Err(format!("{} trailing bytes after the transaction", data.len() - pos));
        }
        let shielded = data[shielded_start..].to_vec();
        Ok(Coinbase { header, version_group_id, script_sig, sequence, vout, lock_time, expiry_height, value_balance, shielded, sapling_outputs })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(200);
        out.extend_from_slice(&self.header.to_le_bytes());
        out.extend_from_slice(&self.version_group_id.to_le_bytes());
        out.push(1);
        out.extend_from_slice(&[0u8; 32]);
        out.extend_from_slice(&[0xff; 4]);
        out.extend(compact_size(self.script_sig.len() as u64));
        out.extend_from_slice(&self.script_sig);
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend(compact_size(self.vout.len() as u64));
        for o in &self.vout {
            out.extend_from_slice(&o.value.to_le_bytes());
            out.extend(compact_size(o.script_pubkey.len() as u64));
            out.extend_from_slice(&o.script_pubkey);
        }
        out.extend_from_slice(&self.lock_time.to_le_bytes());
        out.extend_from_slice(&self.expiry_height.to_le_bytes());
        out.extend_from_slice(&(self.value_balance as u64).to_le_bytes());
        out.extend_from_slice(&self.shielded);
        out
    }

    /// The sum of the transparent outputs, in zatoshi (None on overflow).
    pub fn transparent_total(&self) -> Option<u64> {
        self.vout.iter().try_fold(0u64, |a, o| a.checked_add(o.value))
    }

    /// What the coinbase claims: its transparent outputs plus what it moves into the Sapling pool
    /// (`-valueBalance`), i.e. subsidy plus fees for a coinbase the node built.
    pub fn claimed_total(&self) -> Option<u64> {
        let t = i128::from(self.transparent_total()?) - i128::from(self.value_balance);
        u64::try_from(t).ok()
    }
}

/// Consensus bound on a coinbase scriptSig (`ref/ycash/src/main.cpp:1456-1458`).
pub const MAX_COINBASE_SCRIPTSIG: usize = 100;

/// What `rebuild_script_sig` did to the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    pub script_sig: Vec<u8>,
    /// Bytes of text that fit (the caller warns when this is shorter than the text given).
    pub text_used: usize,
}

/// The `cenote` scriptSig: `height push ‖ coinbaseaux.flags (verbatim) ‖ push(text)`, kept
/// within `MAX_COINBASE_SCRIPTSIG` by truncating the text (`pool/README.md`, carrier 3). The
/// Perl (`ref/yolo/cenote:522`) sliced five bytes and appended the text raw (no push opcode),
/// dropping the flags and with them the Yellowback tag (Y-F1).
///
/// `height_push` is the push the node put at the front of the template's scriptSig (taken from
/// the parsed coinbase, so it is exactly what the node built for this height).
pub fn rebuild_script_sig(height_push: &[u8], flags: &[u8], text: &[u8]) -> Result<Rebuilt, String> {
    let fixed = height_push.len() + flags.len();
    if fixed > MAX_COINBASE_SCRIPTSIG {
        return Err(format!("height push + coinbaseaux.flags is {} bytes, over the {}-byte scriptSig limit", fixed, MAX_COINBASE_SCRIPTSIG));
    }
    let room = MAX_COINBASE_SCRIPTSIG - fixed;
    let mut script_sig = Vec::with_capacity(MAX_COINBASE_SCRIPTSIG);
    script_sig.extend_from_slice(height_push);
    script_sig.extend_from_slice(flags);
    // A direct push costs one byte up to 75 bytes of data, two (OP_PUSHDATA1) beyond.
    let mut text_used = text.len();
    loop {
        let push_hdr = if text_used == 0 {
            0
        } else if text_used <= 75 {
            1
        } else {
            2
        };
        if push_hdr + text_used <= room {
            break;
        }
        text_used -= 1;
    }
    if text_used > 0 {
        if text_used > 75 {
            script_sig.push(0x4c);
        }
        script_sig.push(text_used as u8);
        script_sig.extend_from_slice(&text[..text_used]);
    }
    // A coinbase scriptSig must be at least two bytes (main.cpp:1456); a 1-byte height push
    // with no flags and no text would fail that, so pad with OP_0 as the node does.
    if script_sig.len() < 2 {
        script_sig.push(0x00);
    }
    Ok(Rebuilt { script_sig, text_used })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::{decode_coinbase_tag, height_push_len, tag_kind};

    fn template_coinbase() -> (Vec<u8>, String) {
        let t: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/regtest-template-105.json")).unwrap();
        (hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap(), t["coinbaseaux"]["flags"].as_str().unwrap().to_string())
    }

    #[test]
    fn parse_roundtrip_regtest_coinbase() {
        let (data, flags) = template_coinbase();
        let cb = Coinbase::parse(&data).unwrap();
        assert_eq!(cb.header, 0x8000_0004);
        assert_eq!(cb.version_group_id, SAPLING_VERSION_GROUP_ID);
        assert_eq!(cb.sequence, 0xffff_ffff);
        // this node ran with the 5 % development-fund share (`-ydf=5`, the old default): two outputs
        assert_eq!(cb.vout.len(), 2, "miner output + YDF output");
        assert_eq!(cb.vout[0].value, 593_750_490);
        assert_eq!(cb.vout[1].value, 31_250_000);
        assert_eq!(hex::encode(&cb.vout[1].script_pubkey), "76a91409beeb250c2f6b918dbd5e5a065f5b14d51faea288ac");
        assert_eq!(cb.lock_time, 0);
        assert_eq!(cb.expiry_height, 0);
        assert_eq!(cb.value_balance, 0);
        // scriptSig = 01 69 | 00 | flags
        assert_eq!(hex::encode(&cb.script_sig), format!("016900{}", flags));
        assert_eq!(cb.serialize(), data);
        // the no-flags template too (height 4: OP_4 OP_0)
        let t: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        let data4 = hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap();
        let cb4 = Coinbase::parse(&data4).unwrap();
        assert_eq!(cb4.script_sig, vec![0x54, 0x00]);
        assert_eq!(cb4.serialize(), data4);
    }

    fn vector_coinbase(json: &str) -> Vec<u8> {
        let t: serde_json::Value = serde_json::from_str(json).unwrap();
        hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()
    }

    /// A node at the default `-ydf=0` past the fund's mandate: one output, the whole amount.
    #[test]
    fn parse_coinbase_without_a_fund_output() {
        let data = vector_coinbase(include_str!("../tests/vectors/regtest-template-111-nofund.json"));
        let cb = Coinbase::parse(&data).unwrap();
        assert_eq!(cb.vout.len(), 1, "no fund output");
        assert_eq!(cb.vout[0].value, 625_000_489, "subsidy plus the two fees");
        assert_eq!(cb.sapling_outputs, 0);
        assert_eq!(cb.shielded, vec![0, 0, 0]);
        assert_eq!(cb.claimed_total(), Some(625_000_489));
        assert_eq!(cb.serialize(), data);
    }

    /// A Sapling coinbase (the node's own `-mineraddress=<Sapling>`, the shape
    /// `getcoinbasetemplatefor` returns): no transparent output, one Sapling output, the binding
    /// signature; carried verbatim through a scriptSig rewrite.
    #[test]
    fn parse_sapling_coinbase_opaquely() {
        let data = vector_coinbase(include_str!("../tests/vectors/regtest-template-111-sapling.json"));
        let cb = Coinbase::parse(&data).unwrap();
        assert!(cb.vout.is_empty());
        assert_eq!(cb.sapling_outputs, 1);
        assert_eq!(cb.value_balance, -625_000_489);
        assert_eq!(cb.shielded.len(), 1 + 1 + SAPLING_OUTPUT_SIZE + 1 + BINDING_SIG_SIZE);
        assert_eq!(cb.claimed_total(), Some(625_000_489));
        assert_eq!(cb.serialize(), data);
        let mut longer = cb.clone();
        longer.script_sig.extend_from_slice(b"yolo");
        let out = longer.serialize();
        assert_eq!(out.len(), data.len() + 4);
        let back = Coinbase::parse(&out).unwrap();
        assert_eq!(back.shielded, cb.shielded);
        assert_eq!(back.value_balance, cb.value_balance);
        // with the optional fund share the fund is vout[0], not the miner
        let fund = Coinbase::parse(&vector_coinbase(include_str!("../tests/vectors/regtest-template-111-sapling-fund.json"))).unwrap();
        assert_eq!(fund.vout.len(), 1);
        assert_eq!(fund.vout[0].value, 31_250_000);
        assert_eq!(fund.sapling_outputs, 1);
        assert_eq!(fund.claimed_total(), Some(625_000_000));
        // truncated or padded shielded parts are refused
        assert!(Coinbase::parse(&data[..data.len() - 1]).unwrap_err().contains("truncated"));
        let mut extra = data.clone();
        extra.push(0);
        assert!(Coinbase::parse(&extra).unwrap_err().contains("trailing"));
    }

    #[test]
    fn parse_rejects_non_coinbase_and_garbage() {
        let tx = hex::decode(include_str!("../tests/vectors/regtest-tx-a805.hex").trim()).unwrap();
        assert!(Coinbase::parse(&tx).unwrap_err().contains("prevout not null"));
        let (data, _) = template_coinbase();
        assert!(Coinbase::parse(&data[..50]).is_err());
        let mut extra = data.clone();
        extra.push(0);
        assert!(Coinbase::parse(&extra).unwrap_err().contains("trailing"));
        let mut v3 = data.clone();
        v3[0] = 3;
        assert!(Coinbase::parse(&v3).unwrap_err().contains("not a v4"));
    }

    #[test]
    fn rewrite_payout_output_keeps_scriptsig_and_tag() {
        let (data, _) = template_coinbase();
        let mut cb = Coinbase::parse(&data).unwrap();
        let miner = hex::decode("76a914000102030405060708090a0b0c0d0e0f1011121388ac").unwrap();
        cb.vout[0].script_pubkey = miner.clone();
        let out = cb.serialize();
        assert_eq!(out.len(), data.len());
        let back = Coinbase::parse(&out).unwrap();
        assert_eq!(back.vout[0].script_pubkey, miner);
        assert_eq!(back.vout[0].value, 593_750_490);
        assert_eq!(back.vout[1], cb.vout[1]);
        assert_eq!(tag_kind(&back.script_sig), "quote");
        // the Perl `stratumpool` rewrite is a regex over the hex; the structural rewrite yields
        // the same bytes for this coinbase
        let perl = data.clone();
        let perl_hex = hex::encode(&perl).replacen("76a914b5521b95530df65bec840c03c0e90a126c67625888ac", &hex::encode(&miner), 1);
        assert_eq!(hex::encode(&out), perl_hex);
    }

    #[test]
    fn rebuild_scriptsig_with_flags() {
        let (data, flags) = template_coinbase();
        let cb = Coinbase::parse(&data).unwrap();
        let hp = &cb.script_sig[..height_push_len(&cb.script_sig).unwrap()];
        assert_eq!(hp, [0x01, 0x69]);
        let flags = hex::decode(flags).unwrap();
        let r = rebuild_script_sig(hp, &flags, b"www.FreeSoloMining.com").unwrap();
        assert_eq!(r.text_used, 22);
        assert_eq!(r.script_sig.len(), 2 + 37 + 1 + 22);
        assert_eq!(&r.script_sig[..2], hp);
        assert_eq!(&r.script_sig[2..39], &flags[..]);
        assert_eq!(r.script_sig[39], 22);
        assert_eq!(&r.script_sig[40..], b"www.FreeSoloMining.com");
        let tag = decode_coinbase_tag(&r.script_sig).expect("tag survives the rebuild");
        assert_eq!(tag.price_micro_usd, 123_456);
        assert_eq!(tag.offset, 2);
    }

    #[test]
    fn rebuild_scriptsig_without_flags() {
        let r = rebuild_script_sig(&[0x54], &[], b"hello").unwrap();
        assert_eq!(r.script_sig, vec![0x54, 0x05, b'h', b'e', b'l', b'l', b'o']);
        assert_eq!(tag_kind(&r.script_sig), "none");
        // no flags, no text: still two bytes
        let r = rebuild_script_sig(&[0x54], &[], b"").unwrap();
        assert_eq!(r.script_sig, vec![0x54, 0x00]);
        let r = rebuild_script_sig(&[0x01, 0x69], &[], b"").unwrap();
        assert_eq!(r.script_sig, vec![0x01, 0x69]);
    }

    #[test]
    fn rebuild_scriptsig_at_the_100_byte_boundary() {
        let (_, flags) = template_coinbase();
        let flags = hex::decode(flags).unwrap();
        let hp = [0x03, 0x40, 0x42, 0x0f]; // a mainnet-sized height push
        let text = [b'x'; 200];
        let r = rebuild_script_sig(&hp, &flags, &text).unwrap();
        assert_eq!(r.script_sig.len(), MAX_COINBASE_SCRIPTSIG);
        // 100 - 4 - 37 = 59 bytes of room → 1-byte push header + 58 bytes of text
        assert_eq!(r.text_used, 58);
        assert_eq!(r.script_sig[41], 58);
        assert_eq!(decode_coinbase_tag(&r.script_sig).unwrap().price_micro_usd, 123_456);
        // exactly fits: no truncation
        let r = rebuild_script_sig(&hp, &flags, &text[..58]).unwrap();
        assert_eq!(r.text_used, 58);
        assert_eq!(r.script_sig.len(), 100);
        // one over: truncated by one
        let r = rebuild_script_sig(&hp, &flags, &text[..59]).unwrap();
        assert_eq!(r.text_used, 58);
        // no flags, long text: OP_PUSHDATA1 for > 75 bytes; 100 - 4 = 96 → 2 + 94
        let r = rebuild_script_sig(&hp, &[], &text).unwrap();
        assert_eq!(r.text_used, 94);
        assert_eq!(r.script_sig.len(), 100);
        assert_eq!(r.script_sig[4], 0x4c);
        assert_eq!(r.script_sig[5], 94);
        // 75 bytes of text takes a 1-byte header; 76 takes two
        let r = rebuild_script_sig(&hp, &[], &text[..75]).unwrap();
        assert_eq!(r.script_sig.len(), 4 + 1 + 75);
        let r = rebuild_script_sig(&hp, &[], &text[..76]).unwrap();
        assert_eq!(r.script_sig.len(), 4 + 2 + 76);
        // flags alone over the limit is an error
        assert!(rebuild_script_sig(&hp, &[0u8; 97], b"").is_err());
    }

    #[test]
    fn huge_compact_sizes_are_rejected_not_overflowed() {
        // a coinbase whose scriptSig length claims u64::MAX: an error, not a panic (audit H-17)
        let (data, _) = template_coinbase();
        let mut bad = data[..8 + 1 + 36].to_vec();
        bad.push(0xff);
        bad.extend_from_slice(&u64::MAX.to_le_bytes());
        bad.extend_from_slice(&data[8 + 1 + 36 + 1..]);
        let e = Coinbase::parse(&bad).unwrap_err();
        assert!(e.contains("out of range") || e.contains("truncated"), "{}", e);
        assert!(span(usize::MAX, 1).is_err());
        assert_eq!(span(5, 7).unwrap(), 12);
    }
}

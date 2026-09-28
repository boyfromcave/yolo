//! From a template to a `mining.notify` and back to a `submitblock` (`new_work` and the
//! `mining.submit` branch of `ref/yolo/stratumsolo`), with the coinbase policy of the
//! payout × text flag pair (plan §3.2.4 as revised by Y7).

use crate::codec::{compact_size, dsha256, hash_from_display, merkle_root, reverse_hex, u32_le_hex};
use crate::equihash::Equihash;
use crate::tag::{decode_coinbase_tag, height_push_len, Tag};
use crate::template::BlockTemplate;
use crate::tx::{rebuild_script_sig, Coinbase, MAX_COINBASE_SCRIPTSIG};

/// The coinbase policy: the two flags of plan Y7 (owner decision P-6). Every combination
/// carries the Yellowback tag; there are no modes.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// `--payout`: every block pays this scriptPubKey (validated once at startup). Unset: the
    /// miner's stratum username is the payout address (`BuildParams::miner_script_pubkey`).
    pub payout: Option<Payout>,
    /// `--text`: the scriptSig is rebuilt as height push ‖ `coinbaseaux.flags` ‖ push(text).
    /// Unset: the node's scriptSig is used untouched.
    pub text: Option<Vec<u8>>,
    /// Test-only (Y5's negative case): `--text` without the flags append, reproducing the
    /// Perl `cenote` and dropping the tag (Y-F1).
    pub no_flags: bool,
}

/// A fixed payout address and the scriptPubKey `validateaddress` gave for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payout {
    pub address: String,
    pub script_pubkey: Vec<u8>,
}

impl Policy {
    /// What `/status` reports as `payout`: the fixed address, or `"username"`.
    pub fn payout_label(&self) -> String {
        self.payout.as_ref().map(|p| p.address.clone()).unwrap_or_else(|| "username".into())
    }
}

/// One job, as sent to a miner. Hex fields are exactly the `mining.notify` params.
#[derive(Debug, Clone)]
pub struct Work {
    pub job_id: String,
    pub height: u32,
    pub version: String,
    pub previousblockhash: String,
    pub merkleroot: String,
    pub light_client_root: String,
    pub time: String,
    pub bits: String,
    pub target: String,
    /// compact tx count ‖ coinbase ‖ selected transactions, hex.
    pub transactions: String,
    pub transaction_count: usize,
    pub coinbase_script_sig: Vec<u8>,
    pub tag: Option<Tag>,
    pub text_truncated_to: Option<usize>,
}

impl Work {
    pub fn tag_kind(&self) -> &'static str {
        self.tag.as_ref().map(|t| t.kind()).unwrap_or("none")
    }
}

/// Inputs that vary per client / per build.
#[derive(Debug, Clone, Default)]
pub struct BuildParams<'a> {
    pub job_id: String,
    /// The miner's `validateaddress.scriptPubKey` from `mining.authorize`; used when the
    /// policy has no fixed `--payout`.
    pub miner_script_pubkey: Option<&'a [u8]>,
    /// Unix time the job was created (Perl: `time` at `new_work`).
    pub now: u32,
}

const HEADER_SIZE: usize = 4 + 32 + 32 + 32 + 4 + 4 + 32;
const DEFAULT_SIZE_LIMIT: usize = 2_000_000;

/// The coinbase bytes under the policy, and the scriptSig they carry.
pub struct BuiltCoinbase {
    pub data: Vec<u8>,
    pub script_sig: Vec<u8>,
    /// Set when the `--text` had to be truncated (to this many bytes).
    pub text_truncated_to: Option<usize>,
}

pub fn build_coinbase(t: &BlockTemplate, policy: &Policy, p: &BuildParams) -> Result<BuiltCoinbase, String> {
    let data = hex::decode(&t.coinbasetxn.data).map_err(|e| format!("coinbasetxn.data is not hex: {}", e))?;
    let mut cb = Coinbase::parse(&data).map_err(|e| format!("coinbasetxn.data: {}", e))?;
    if cb.vout.is_empty() {
        return Err("coinbase has no outputs".into());
    }
    let mut truncated = None;
    if let Some(text) = &policy.text {
        let hp_len = height_push_len(&cb.script_sig).ok_or("coinbase scriptSig does not start with a height push")?;
        let height_push = cb.script_sig[..hp_len].to_vec();
        let flags = if policy.no_flags { Vec::new() } else { t.flags_bytes()? };
        let rebuilt = rebuild_script_sig(&height_push, &flags, text)?;
        if rebuilt.text_used < text.len() {
            truncated = Some(rebuilt.text_used);
        }
        cb.script_sig = rebuilt.script_sig;
    }
    debug_assert!(cb.script_sig.len() <= MAX_COINBASE_SCRIPTSIG);
    // The miner's reward is vout[0] (`CreateCoinbaseTransaction`, `ycash-dd/src/miner.cpp:304`);
    // any further output is the founders/YDF share and stays as the node built it. The
    // output is rewritten structurally in both payout cases: `--payout <the node's own
    // address>` is what the Perl `stratumsolo` was.
    let spk = match &policy.payout {
        Some(fixed) => fixed.script_pubkey.as_slice(),
        None => p.miner_script_pubkey.ok_or("no --payout: the miner's scriptPubKey is needed (authorize first)")?,
    };
    cb.vout[0].script_pubkey = spk.to_vec();
    let script_sig = cb.script_sig.clone();
    Ok(BuiltCoinbase { data: cb.serialize(), script_sig, text_truncated_to: truncated })
}

/// `new_work`: coinbase per policy, transactions under the size limit, merkle root, header
/// fields byte-reversed as hex the way the Perl sends them.
pub fn build_work(t: &BlockTemplate, policy: &Policy, equihash: Equihash, p: &BuildParams) -> Result<Work, String> {
    let BuiltCoinbase { data: coinbase, script_sig, text_truncated_to: truncated } = build_coinbase(t, policy, p)?;
    let limit = t.sizelimit.unwrap_or(DEFAULT_SIZE_LIMIT);
    let mut txids: Vec<[u8; 32]> = vec![dsha256(&coinbase)];
    let mut txs_hex = hex::encode(&coinbase);
    let mut size = HEADER_SIZE + equihash.solution_wire_len() + 9 + coinbase.len();
    for tx in &t.transactions {
        let tx_size = tx.data.len() / 2;
        if size + tx_size >= limit {
            break;
        }
        txids.push(hash_from_display(&tx.hash)?);
        txs_hex.push_str(&tx.data);
        size += tx_size;
    }
    let count = txids.len();
    let root = merkle_root(&txids);
    let version = u32::try_from(t.version).map_err(|_| format!("template version {} out of range", t.version))?;
    Ok(Work {
        job_id: p.job_id.clone(),
        height: t.height,
        version: u32_le_hex(version),
        previousblockhash: reverse_hex(&t.previousblockhash),
        merkleroot: hex::encode(root),
        light_client_root: reverse_hex(t.light_client_root()),
        time: u32_le_hex(p.now),
        bits: reverse_hex(&t.bits),
        target: t.target.clone(),
        transactions: format!("{}{}", hex::encode(compact_size(count as u64)), txs_hex),
        transaction_count: count,
        tag: decode_coinbase_tag(&script_sig),
        coinbase_script_sig: script_sig,
        text_truncated_to: truncated,
    })
}

/// Checked `mining.submit` params: ntime, nonce2 and solution as hex.
#[derive(Debug, Clone)]
pub struct Submit {
    pub ntime: String,
    pub nonce2: String,
    pub solution: String,
}

impl Submit {
    pub fn check(ntime: &str, nonce2: &str, solution: &str, nonce1_hex_len: usize, equihash: Equihash) -> Result<Submit, String> {
        let is_hex = |s: &str| !s.is_empty() && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit());
        if ntime.len() != 8 || !is_hex(ntime) {
            return Err(format!("ntime {:?} is not 4 bytes of hex", ntime));
        }
        if nonce2.len() != 64 - nonce1_hex_len || !is_hex(nonce2) {
            return Err(format!("nonce2 {:?} is not {} hex chars", nonce2, 64 - nonce1_hex_len));
        }
        equihash.check_solution_hex(solution)?;
        Ok(Submit { ntime: ntime.to_ascii_lowercase(), nonce2: nonce2.to_ascii_lowercase(), solution: solution.to_ascii_lowercase() })
    }
}

/// The raw block for `submitblock`, byte for byte as the Perl concatenates it: header fields
/// from the job, `nTime` as the miner echoed it, nonce = nonce1 ‖ nonce2, the solution as sent
/// (compact size included), then the transactions.
pub fn assemble_block(work: &Work, nonce1: &str, s: &Submit) -> String {
    let mut block = String::with_capacity(work.transactions.len() + 400);
    block.push_str(&work.version);
    block.push_str(&work.previousblockhash);
    block.push_str(&work.merkleroot);
    block.push_str(&work.light_client_root);
    block.push_str(&s.ntime);
    block.push_str(&work.bits);
    block.push_str(nonce1);
    block.push_str(&s.nonce2);
    block.push_str(&s.solution);
    block.push_str(&work.transactions);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::tag_kind;

    fn template() -> BlockTemplate {
        serde_json::from_str(include_str!("../tests/vectors/regtest-template-105.json")).unwrap()
    }

    const MINER_SPK: &str = "76a914000102030405060708090a0b0c0d0e0f1011121388ac";
    /// The template's own payout script (node A's `mineraddress`): `--payout <the node's
    /// address>` reproduces the Perl `stratumsolo` coinbase byte for byte.
    const NODE_SPK: &str = "76a914b5521b95530df65bec840c03c0e90a126c67625888ac";
    const TEXT: &[u8] = b"www.FreeSoloMining.com";

    fn fixed(spk_hex: &str) -> Option<Payout> {
        Some(Payout { address: "fixed".into(), script_pubkey: hex::decode(spk_hex).unwrap() })
    }

    /// One cell of the payout × text grid.
    fn policy(payout: Option<Payout>, text: Option<&[u8]>) -> Policy {
        Policy { payout, text: text.map(|t| t.to_vec()), no_flags: false }
    }

    fn params<'a>(spk: Option<&'a [u8]>) -> BuildParams<'a> {
        BuildParams { job_id: "1".into(), miner_script_pubkey: spk, now: 1_790_581_942 }
    }

    /// The coinbase `build_work` embedded, parsed back (its hex is `transactions[2..]`'s prefix).
    fn coinbase_of(w: &Work, t: &BlockTemplate, pol: &Policy, p: &BuildParams) -> Coinbase {
        let built = build_coinbase(t, pol, p).unwrap();
        assert!(w.transactions[2..].starts_with(&hex::encode(&built.data)));
        Coinbase::parse(&built.data).unwrap()
    }

    #[test]
    fn header_fields_are_reversed_like_the_perl() {
        let t = template();
        let w = build_work(&t, &policy(fixed(NODE_SPK), None), Equihash::REGTEST, &params(None)).unwrap();
        assert_eq!(w.version, "04000000");
        assert_eq!(w.previousblockhash, "f9e5cdb460c2f54266fe370a80c9accf0984bf5f0c9f7f6c0d4986db8cb4f904");
        assert_eq!(w.light_client_root, "5347459ce3ba2a20a698da41174e76c9d15e88b9ee2f25507915a934961b5ab9");
        assert_eq!(w.bits, "0f0f0f20");
        assert_eq!(w.time, "b61cba6a");
        assert_eq!(w.target, t.target);
        assert_eq!(w.transaction_count, 3);
        assert!(w.transactions.starts_with("03"));
        assert!(w.transactions[2..].starts_with(&t.coinbasetxn.data));
        assert!(w.transactions.ends_with(&t.transactions[1].data));
        assert_eq!(w.tag_kind(), "quote");
        // --payout <the node's own address>, no --text: the coinbase is byte for byte the node's
        assert_eq!(&w.transactions[2..2 + t.coinbasetxn.data.len()], t.coinbasetxn.data);
        // the header prefix matches the mined block 105 up to the merkle root (different coinbase)
        let raw = include_str!("../tests/vectors/regtest-block-105.hex").trim();
        assert_eq!(&raw[..72], format!("{}{}", w.version, w.previousblockhash));
        assert_eq!(&raw[136..200], w.light_client_root);
        assert_eq!(&raw[208..216], w.bits);
    }

    #[test]
    fn assembled_block_reproduces_block_105_layout() {
        // Block 105's own transactions with its own coinbase: the assembled bytes equal the
        // node's raw block exactly (nonce1 = first 16 bytes of the raw nonce field).
        let raw = include_str!("../tests/vectors/regtest-block-105.hex").trim();
        let t = template();
        let mut t2 = t.clone();
        // the coinbase the internal miner produced for block 105 is after the header
        let cb_start = 2 * (HEADER_SIZE + 37 + 1);
        let cb_end = raw.find(include_str!("../tests/vectors/regtest-tx-a805.hex").trim()).unwrap();
        t2.coinbasetxn.data = raw[cb_start..cb_end].to_string();
        // the internal miner's coinbase carries an extranonce push instead of OP_0
        let mined = Coinbase::parse(&hex::decode(&t2.coinbasetxn.data).unwrap()).unwrap();
        assert_eq!(mined.script_sig[..4], [0x01, 0x69, 0x01, 0x01]);
        // --payout <the address block 105 paid>: the structural rewrite is a no-op
        let payout = Some(Payout { address: "block-105".into(), script_pubkey: mined.vout[0].script_pubkey.clone() });
        let w = build_work(&t2, &policy(payout, None), Equihash::REGTEST, &params(None)).unwrap();
        assert_eq!(w.merkleroot, &raw[72..136]);
        let nonce_hex = &raw[216..280];
        let s = Submit::check(&raw[200..208], &nonce_hex[28..], &raw[280..354], 28, Equihash::REGTEST).unwrap();
        assert_eq!(assemble_block(&w, &nonce_hex[..28], &s), raw);
    }

    #[test]
    fn payout_unset_pays_the_username_and_keeps_the_tag() {
        let t = template();
        let spk = hex::decode(MINER_SPK).unwrap();
        let pol = policy(None, None);
        let w = build_work(&t, &pol, Equihash::REGTEST, &params(Some(&spk))).unwrap();
        let cb = coinbase_of(&w, &t, &pol, &params(Some(&spk)));
        assert_eq!(cb.vout[0].script_pubkey, spk);
        assert_eq!(cb.vout[0].value, 593_750_490);
        assert_eq!(cb.vout.len(), 2);
        assert_eq!(cb.script_sig, Coinbase::parse(&hex::decode(&t.coinbasetxn.data).unwrap()).unwrap().script_sig, "no --text: scriptSig untouched");
        assert_eq!(w.tag_kind(), "quote");
        assert_ne!(w.merkleroot, build_work(&t, &policy(fixed(NODE_SPK), None), Equihash::REGTEST, &params(None)).unwrap().merkleroot);
        // no --payout and no authorized address: no work
        assert!(build_work(&t, &policy(None, None), Equihash::REGTEST, &params(None)).is_err());
    }

    #[test]
    fn payout_set_ignores_the_username() {
        let t = template();
        let miner = hex::decode(MINER_SPK).unwrap();
        for text in [None, Some(TEXT)] {
            let pol = policy(fixed(MINER_SPK), text);
            let w = build_work(&t, &pol, Equihash::REGTEST, &params(None)).unwrap();
            let cb = coinbase_of(&w, &t, &pol, &params(None));
            assert_eq!(cb.vout[0].script_pubkey, miner);
            assert_eq!(cb.vout[0].value, 593_750_490);
            assert_eq!(w.tag_kind(), "quote");
            // the miner's own address does not change the block
            let w2 = build_work(&t, &policy(fixed(MINER_SPK), text), Equihash::REGTEST, &params(Some(&hex::decode(NODE_SPK).unwrap()))).unwrap();
            assert_eq!(w2.merkleroot, w.merkleroot);
        }
    }

    #[test]
    fn text_set_carries_flags_and_text() {
        let t = template();
        let spk = hex::decode(MINER_SPK).unwrap();
        for payout in [None, fixed(NODE_SPK)] {
            let pol = policy(payout.clone(), Some(TEXT));
            let w = build_work(&t, &pol, Equihash::REGTEST, &params(Some(&spk))).unwrap();
            assert_eq!(w.tag_kind(), "quote");
            assert_eq!(w.text_truncated_to, None);
            // the rebuilt scriptSig is 2 + 37 + 1 + 22 bytes against the template's 2 + 1 + 37
            let sig = &w.coinbase_script_sig;
            assert_eq!(sig.len(), 2 + 37 + 1 + 22);
            assert_eq!(&sig[..2], [0x01, 0x69]);
            assert_eq!(&sig[2..39], &t.flags_bytes().unwrap()[..]);
            assert_eq!(&sig[40..], TEXT);
            let cb = coinbase_of(&w, &t, &pol, &params(Some(&spk)));
            assert_eq!(cb.script_sig, *sig);
            assert_eq!(cb.vout[0].value, 593_750_490);
            let want = if payout.is_some() { hex::decode(NODE_SPK).unwrap() } else { spk.clone() };
            assert_eq!(cb.vout[0].script_pubkey, want);
            assert_eq!(tag_kind(&cb.script_sig), "quote");
        }
        // the Perl's behaviour (no flags) drops the tag: Y-F1
        let mut pol = policy(None, Some(TEXT));
        pol.no_flags = true;
        let w = build_work(&t, &pol, Equihash::REGTEST, &params(Some(&spk))).unwrap();
        assert_eq!(w.tag_kind(), "none");
        assert_eq!(&w.coinbase_script_sig[..2], [0x01, 0x69]);
        assert_eq!(&w.coinbase_script_sig[3..], TEXT);
        // long text is truncated and reported
        let long = vec![b'y'; 120];
        let w = build_work(&t, &policy(None, Some(&long)), Equihash::REGTEST, &params(Some(&spk))).unwrap();
        assert_eq!(w.coinbase_script_sig.len(), MAX_COINBASE_SCRIPTSIG);
        assert_eq!(w.text_truncated_to, Some(100 - 2 - 37 - 1));
        assert_eq!(w.tag_kind(), "quote");
        // empty text: height push ‖ flags, nothing pushed
        let w = build_work(&t, &policy(None, Some(b"")), Equihash::REGTEST, &params(Some(&spk))).unwrap();
        assert_eq!(w.coinbase_script_sig.len(), 2 + 37);
        assert_eq!(w.tag_kind(), "quote");
    }

    #[test]
    fn plain_node_without_flags_behaves_like_the_perl() {
        let t: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        let spk = hex::decode(MINER_SPK).unwrap();
        for payout in [None, fixed(NODE_SPK)] {
            for text in [None, Some(TEXT)] {
                let w = build_work(&t, &policy(payout.clone(), text), Equihash::REGTEST, &params(Some(&spk))).unwrap();
                assert_eq!(w.tag_kind(), "none");
                assert_eq!(w.transaction_count, 1);
            }
        }
        let w = build_work(&t, &policy(None, Some(TEXT)), Equihash::REGTEST, &params(Some(&spk))).unwrap();
        assert_eq!(w.coinbase_script_sig[0], 0x54);
        assert_eq!(&w.coinbase_script_sig[2..], TEXT);
    }

    #[test]
    fn size_limit_stops_selection() {
        let mut t = template();
        t.sizelimit = Some(HEADER_SIZE + 37 + 9 + t.coinbasetxn.data.len() / 2 + 245 + 10);
        let w = build_work(&t, &policy(fixed(NODE_SPK), None), Equihash::REGTEST, &params(None)).unwrap();
        assert_eq!(w.transaction_count, 2);
    }

    #[test]
    fn payout_label_for_status() {
        assert_eq!(policy(None, None).payout_label(), "username");
        assert_eq!(policy(fixed(NODE_SPK), None).payout_label(), "fixed");
    }

    #[test]
    fn submit_checks() {
        let sol = format!("24{}", "ab".repeat(36));
        // the Perl's 28-char nonce1 leaves 36 hex chars of nonce2
        assert!(Submit::check("b61cba6a", &"0".repeat(36), &sol, 28, Equihash::REGTEST).is_ok());
        assert!(Submit::check("b61cba6", &"0".repeat(36), &sol, 28, Equihash::REGTEST).is_err());
        assert!(Submit::check("b61cba6a", &"0".repeat(32), &sol, 28, Equihash::REGTEST).is_err());
        assert!(Submit::check("b61cba6a", &"0".repeat(36), &sol, 28, Equihash::MAINNET).is_err());
        assert!(Submit::check("b61cba6a", &"g".repeat(36), &sol, 28, Equihash::REGTEST).is_err());
    }
}

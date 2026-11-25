// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! `getblocktemplate` as yolo reads it, and the change key that decides when miners get new
//! work (height, target, header root and, unlike the Perl, `coinbaseaux.flags`: Y-F2).
//!
//! The header's fourth field (`hashLightClientRoot` on ycashd v4.5.0, `hashBlockCommitments` on
//! 6.20.0; the same bytes before NU5) is read from whichever key the node serves; see
//! [`BlockTemplate::header_root`].

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TemplateTx {
    pub data: String,
    pub hash: String,
    /// The transaction's fee in zatoshi, as the node's mempool entry states it (what the
    /// coinbase may claim for it). Checked against `getcoinbasetemplatefor`'s `fees`.
    #[serde(default)]
    pub fee: Option<i64>,
    /// The transaction's signature operations, as the node counts them toward `sigoplimit`.
    #[serde(default)]
    pub sigops: Option<i64>,
    /// Yugen (branch id `0x59474e30`): the proofs the transaction commits to, hex, in its own
    /// order. They are block data beside the transaction (yugen-spec 0.5.1 §15.1, "The proofs
    /// are block data"), not part of `data`; absent for every other transaction.
    #[serde(default)]
    pub proofs: Vec<String>,
}

impl TemplateTx {
    /// The transaction's bytes in the block.
    pub fn size(&self) -> usize {
        self.data.len() / 2
    }

    /// The bytes of the proofs it commits to.
    pub fn proof_bytes(&self) -> usize {
        self.proofs.iter().map(|p| p.len() / 2).sum()
    }

    /// Its weight (yugen-spec 0.5.1 §16 W3): its bytes plus the bytes of every proof it
    /// commits to. Equal to its size for a transaction without proofs.
    pub fn weight(&self) -> usize {
        self.size() + self.proof_bytes()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoinbaseTxn {
    pub data: String,
    #[allow(dead_code)]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CoinbaseAux {
    #[serde(default)]
    pub flags: String,
}

/// `defaultroots` (ycashd 6.20.0): roots valid for the template used unmodified.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DefaultRoots {
    /// Always sent; all zeros before Heartwood and at its activation height.
    pub chainhistoryroot: Option<String>,
    /// Sent only from NU5: the header's `hashBlockCommitments`.
    pub blockcommitmentshash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlockTemplate {
    pub version: i64,
    pub previousblockhash: String,
    /// The header's `hashLightClientRoot`; the deprecated `finalsaplingroothash` carries the
    /// same value (`ycash-dd/src/rpc/mining.cpp:759-761`). Older nodes only send the latter.
    /// On 6.20.0 both, and `blockcommitmentshash`, are the header's `hashBlockCommitments`, sent
    /// only while the `gbt_oldhashes` deprecation is allowed (the default).
    pub lightclientroothash: Option<String>,
    pub finalsaplingroothash: Option<String>,
    pub blockcommitmentshash: Option<String>,
    /// Absent before 6.20.0.
    #[serde(default)]
    pub defaultroots: Option<DefaultRoots>,
    /// Each transaction behind its own `Arc`, so templates polled one after another share the
    /// transactions they have in common ([`BlockTemplate::share_transactions`]): a pair pins its
    /// template (`crate::pairs::Pair`), and a full block's hex is megabytes.
    #[serde(default)]
    pub transactions: Vec<Arc<TemplateTx>>,
    pub coinbasetxn: CoinbaseTxn,
    /// Absent on a node without `-yellowback` when `coinbasetxn` is served: treated as empty.
    #[serde(default)]
    pub coinbaseaux: CoinbaseAux,
    pub target: String,
    pub bits: String,
    pub height: u32,
    pub curtime: Option<i64>,
    pub sizelimit: Option<usize>,
    /// Yugen: `BLOCK_WEIGHT_MAX` (4,000,000; yugen-spec 0.5.1 §16 W3), sent once Yugen is active
    /// for the next block. Absent: the block has no weight rule beyond `sizelimit`.
    #[serde(default)]
    pub weightlimit: Option<usize>,
    /// The block's signature-operation limit (`MAX_BLOCK_SIGOPS`); the coinbase counts toward it.
    #[serde(default)]
    pub sigoplimit: Option<u64>,
}

/// What must change for miners to receive new work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeKey {
    pub height: u32,
    pub target: String,
    /// The header root, or empty when the template does not determine it.
    pub light_client_root: String,
    pub flags: String,
    /// Digest of the template's transaction ids, in order (a transaction arriving mid-job is
    /// mined in the next block, not one later). The poller rate-limits refreshes on this alone.
    pub txset: String,
}

/// How often a change of the transaction set alone sends new work, at most.
pub const TXSET_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.is_empty())
}

impl BlockTemplate {
    /// The header's fourth field as the node's own template sets it (ycash6 `src/miner.cpp`
    /// `CreateNewBlock`, header fill), display hex:
    ///
    /// 1. `defaultroots.blockcommitmentshash`: NU5 and later, the exact header value.
    /// 2. `lightclientroothash` / `blockcommitmentshash` / `finalsaplingroothash`: the exact
    ///    header value on v4.5.0 always, on 6.20.0 while `gbt_oldhashes` is allowed.
    /// 3. `defaultroots.chainhistoryroot` when not all zeros: Heartwood to NU5, where the header
    ///    field is the chain history root (`-allowdeprecated=none` on 6.20.0).
    /// 4. Otherwise an error. An all-zero `chainhistoryroot` is ambiguous (F-29): at Heartwood's
    ///    activation height the header field is zeros, but before Heartwood it is the final
    ///    Sapling root, which `defaultroots` does not carry. Building on zeros there would make
    ///    every share an invalid block, so no work is built. Only a pre-Heartwood chain (regtest)
    ///    with `gbt_oldhashes` disabled reaches this.
    pub fn header_root(&self) -> Result<&str, String> {
        let roots = self.defaultroots.as_ref();
        if let Some(r) = roots.and_then(|r| non_empty(&r.blockcommitmentshash)) {
            return Ok(r);
        }
        if let Some(r) = non_empty(&self.lightclientroothash).or_else(|| non_empty(&self.blockcommitmentshash)).or_else(|| non_empty(&self.finalsaplingroothash)) {
            return Ok(r);
        }
        match roots.and_then(|r| non_empty(&r.chainhistoryroot)) {
            Some(r) if r.bytes().any(|b| b != b'0') => Ok(r),
            Some(_) => Err("template has no header root: defaultroots.chainhistoryroot is null (before \
                 Heartwood or at its activation) and the deprecated root keys are withheld; allow \
                 them with -allowdeprecated=gbt_oldhashes"
                .into()),
            None => Err("template has no header root (no lightclientroothash, finalsaplingroothash, \
                 blockcommitmentshash or defaultroots)"
                .into()),
        }
    }

    pub fn change_key(&self) -> ChangeKey {
        ChangeKey {
            height: self.height,
            target: self.target.clone(),
            light_client_root: self.header_root().unwrap_or("").to_string(),
            flags: self.coinbaseaux.flags.clone(),
            txset: hex::encode(crate::codec::dsha256(self.txids().join(",").as_bytes())),
        }
    }

    /// The template's transaction ids in its order: the reference `getcoinbasetemplatefor` names
    /// the template by, with `previousblockhash`.
    pub fn txids(&self) -> Vec<String> {
        self.transactions.iter().map(|t| t.hash.to_ascii_lowercase()).collect()
    }

    /// The sum of `transactions[].fee`; an error when a fee is missing or negative.
    pub fn fee_total(&self) -> Result<u64, String> {
        self.transactions.iter().try_fold(0u64, |sum, t| match t.fee {
            Some(f) if f >= 0 => sum.checked_add(f as u64).ok_or_else(|| "fee total overflows".to_string()),
            Some(f) => Err(format!("transaction {} has a negative fee {}", t.hash, f)),
            None => Err(format!("transaction {} has no fee in the template", t.hash)),
        })
    }

    /// The sum of `transactions[].sigops`; an error when one is missing or negative.
    pub fn sigop_total(&self) -> Result<u64, String> {
        self.transactions.iter().try_fold(0u64, |sum, t| match t.sigops {
            Some(n) if n >= 0 => sum.checked_add(n as u64).ok_or_else(|| "sigop total overflows".to_string()),
            Some(n) => Err(format!("transaction {} has a negative sigop count {}", t.hash, n)),
            None => Err(format!("transaction {} has no sigop count in the template", t.hash)),
        })
    }

    /// Makes every transaction of this template that `previous` carries too, byte for byte (the
    /// same id, data, fee and sigop count), the very allocation `previous` holds, so a template
    /// polled again with an unchanged transaction set costs no new copy of it. Only identical
    /// values are shared: nothing a job reads from this template changes.
    pub fn share_transactions(&mut self, previous: &BlockTemplate) {
        if previous.transactions.is_empty() {
            return;
        }
        let known: HashMap<&str, &Arc<TemplateTx>> = previous.transactions.iter().map(|t| (t.hash.as_str(), t)).collect();
        for tx in self.transactions.iter_mut() {
            if let Some(old) = known.get(tx.hash.as_str()) {
                if !Arc::ptr_eq(old, tx) && ***old == **tx {
                    *tx = Arc::clone(old);
                }
            }
        }
    }

    pub fn flags_bytes(&self) -> Result<Vec<u8>, String> {
        hex::decode(&self.coinbaseaux.flags).map_err(|e| format!("coinbaseaux.flags is not hex: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vectors_and_change_key() {
        let t: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-105.json")).unwrap();
        assert_eq!(t.height, 105);
        assert_eq!(t.transactions.len(), 2);
        assert_eq!(t.flags_bytes().unwrap().len(), 37);
        assert_eq!(t.header_root().unwrap(), "b95a1b9634a9157950252feeb9885ed1c9764e1741da98a6202abae39c454753");
        assert_eq!(t.fee_total().unwrap(), 490);
        assert_eq!(t.txids(), vec!["a80555562c7669ae53157f380e77e40a0a60e6b5066c3c84122ecf3d3f44f331", "fb9d25619e7c33e39b6201184641cb411bda65bba9ea05e4da114278dd5fbe40"]);
        let mut no_fee = t.clone();
        Arc::make_mut(&mut no_fee.transactions[1]).fee = None;
        assert!(no_fee.fee_total().is_err());
        // a re-polled template shares the transactions that are byte-for-byte the same
        let mut again = t.clone();
        again.transactions = again.transactions.iter().map(|x| Arc::new((**x).clone())).collect();
        let mut changed = (*again.transactions[1]).clone();
        changed.fee = Some(changed.fee.unwrap() + 1);
        again.transactions[1] = Arc::new(changed);
        again.share_transactions(&t);
        assert!(Arc::ptr_eq(&again.transactions[0], &t.transactions[0]));
        assert!(!Arc::ptr_eq(&again.transactions[1], &t.transactions[1]), "a different fee is not the same transaction entry");
        assert_eq!(again.transactions[1].fee, Some(t.transactions[1].fee.unwrap() + 1));
        assert_eq!(t.sigop_total().unwrap(), 4);
        assert_eq!(t.sigoplimit, Some(20000));
        let k = t.change_key();
        let t4: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        assert_eq!(t4.flags_bytes().unwrap(), Vec::<u8>::new());
        assert_ne!(k, t4.change_key());
        // only the flags differ → still a new key (Y-F2)
        let mut t2 = t.clone();
        t2.coinbaseaux.flags = String::new();
        assert_ne!(t2.change_key(), k);
        // only the transaction set differs → a new key (its txset alone)
        let mut t3 = t.clone();
        t3.transactions.pop();
        let k3 = t3.change_key();
        assert_ne!(k3, k);
        assert_eq!((k3.height, &k3.target, &k3.flags), (k.height, &k.target, &k.flags));
        // a node where Yellowback is not live (no vault upgrade or no YED attestor set) omits coinbaseaux in coinbasetxn mode
        let mut v: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        v.as_object_mut().unwrap().remove("coinbaseaux");
        v.as_object_mut().unwrap().remove("lightclientroothash");
        let t: BlockTemplate = serde_json::from_value(v).unwrap();
        assert_eq!(t.coinbaseaux.flags, "");
        assert_eq!(t.header_root().unwrap(), "a7781dd48ddbcf3dca287f0cbdcd571d9003266c40c2c6adba6605404f407e9f");
    }

    fn vector(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    fn strip_old_hashes(mut v: serde_json::Value) -> serde_json::Value {
        let o = v.as_object_mut().unwrap();
        for k in ["lightclientroothash", "finalsaplingroothash", "blockcommitmentshash"] {
            o.remove(k);
        }
        v
    }

    fn root_of(v: serde_json::Value) -> Result<String, String> {
        let t: BlockTemplate = serde_json::from_value(v).unwrap();
        t.header_root().map(String::from)
    }

    const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// ycashd v4.5.0 (ycash-dd): `lightclientroothash`, no `defaultroots`.
    #[test]
    fn header_root_v4_5_0() {
        let v = vector(include_str!("../tests/vectors/regtest-template-105.json"));
        assert!(v.get("defaultroots").is_none());
        assert_eq!(root_of(v).unwrap(), "b95a1b9634a9157950252feeb9885ed1c9764e1741da98a6202abae39c454753");
    }

    /// ycashd 6.20.0 before Heartwood (the Overwinter+Sapling regtest suites): the header field is
    /// the final Sapling root, `chainhistoryroot` is zeros (F-29).
    #[test]
    fn header_root_6_20_0_pre_heartwood() {
        let v = vector(include_str!("../tests/vectors/ycash6-template-pre-heartwood.json"));
        let sapling = "3e49b5f954aa9d3545bc6c37744661eea48d7c34e3000d82b7f0010c30f4c2fb";
        assert_eq!(v["defaultroots"]["chainhistoryroot"], ZEROS);
        assert_eq!(root_of(v.clone()).unwrap(), sapling, "the Sapling root, not the zero chainhistoryroot");
        // -allowdeprecated=none: the Sapling root is not served anywhere, so no work.
        let err = root_of(strip_old_hashes(v)).unwrap_err();
        assert!(err.contains("gbt_oldhashes"), "{}", err);
    }

    /// ycashd 6.20.0 at Heartwood's activation height: the header field is zeros.
    #[test]
    fn header_root_6_20_0_heartwood_activation() {
        let v = vector(include_str!("../tests/vectors/ycash6-template-heartwood-activation.json"));
        assert_eq!(root_of(v.clone()).unwrap(), ZEROS);
        assert!(root_of(strip_old_hashes(v)).is_err(), "zeros alone are ambiguous with pre-Heartwood");
    }

    /// ycashd 6.20.0 after Heartwood, before NU5 (mainnet today): the chain history root, from the
    /// deprecated keys or, with them withheld, from `defaultroots`.
    #[test]
    fn header_root_6_20_0_post_heartwood() {
        let v = vector(include_str!("../tests/vectors/ycash6-template-post-heartwood.json"));
        let history = "710b468ba944823f21655758448348d8cab1c0ada5455b130bedd6e34c0bfc67";
        assert_eq!(v["defaultroots"]["chainhistoryroot"], history);
        assert_eq!(root_of(v.clone()).unwrap(), history);
        assert_eq!(root_of(strip_old_hashes(v)).unwrap(), history);
        // as served by a node started with -allowdeprecated=none
        let v = vector(include_str!("../tests/vectors/ycash6-template-post-heartwood-no-oldhashes.json"));
        assert!(v.get("lightclientroothash").is_none());
        assert_eq!(root_of(v).unwrap(), "472d5adb07e6fc20f1dd5b635a1c356a4b740ef7882d439dad3568906a00f078");
    }

    /// ycashd 6.20.0 from NU5 (not active on Ycash): `defaultroots.blockcommitmentshash`, not the
    /// chain history root it commits to.
    #[test]
    fn header_root_6_20_0_nu5() {
        let mut v = strip_old_hashes(vector(include_str!("../tests/vectors/ycash6-template-post-heartwood.json")));
        let commitments = "11".repeat(32);
        v["defaultroots"]["authdataroot"] = serde_json::json!("22".repeat(32));
        v["defaultroots"]["blockcommitmentshash"] = serde_json::json!(commitments);
        assert_eq!(root_of(v.clone()).unwrap(), commitments);
        let k = serde_json::from_value::<BlockTemplate>(v).unwrap().change_key();
        assert_eq!(k.light_client_root, commitments);
    }
}

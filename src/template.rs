// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! `getblocktemplate` as yolo reads it, and the change key that decides when miners get new
//! work (height, target, header root and, unlike the Perl, `coinbaseaux.flags`: Y-F2).
//!
//! The header's fourth field (`hashLightClientRoot` on ycashd v4.5.0, `hashBlockCommitments` on
//! 6.20.0; the same bytes before NU5) is read from whichever key the node serves; see
//! [`BlockTemplate::header_root`].

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TemplateTx {
    pub data: String,
    pub hash: String,
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
    #[serde(default)]
    pub transactions: Vec<TemplateTx>,
    pub coinbasetxn: CoinbaseTxn,
    /// Absent on a node without `-yellowback` when `coinbasetxn` is served: treated as empty.
    #[serde(default)]
    pub coinbaseaux: CoinbaseAux,
    pub target: String,
    pub bits: String,
    pub height: u32,
    pub curtime: Option<i64>,
    pub sizelimit: Option<usize>,
}

/// What must change for miners to receive new work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeKey {
    pub height: u32,
    pub target: String,
    /// The header root, or empty when the template does not determine it.
    pub light_client_root: String,
    pub flags: String,
}

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
        ChangeKey { height: self.height, target: self.target.clone(), light_client_root: self.header_root().unwrap_or("").to_string(), flags: self.coinbaseaux.flags.clone() }
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
        let k = t.change_key();
        let t4: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        assert_eq!(t4.flags_bytes().unwrap(), Vec::<u8>::new());
        assert_ne!(k, t4.change_key());
        // only the flags differ → still a new key (Y-F2)
        let mut t2 = t.clone();
        t2.coinbaseaux.flags = String::new();
        assert_ne!(t2.change_key(), k);
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

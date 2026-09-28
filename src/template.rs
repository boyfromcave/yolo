//! `getblocktemplate` as yolo reads it, and the change key that decides when miners get new
//! work (height, target, light-client root and, unlike the Perl, `coinbaseaux.flags`: Y-F2).

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

#[derive(Debug, Clone, Deserialize)]
pub struct BlockTemplate {
    pub version: i64,
    pub previousblockhash: String,
    /// The header's `hashLightClientRoot`; the deprecated `finalsaplingroothash` carries the
    /// same value (`ycash-dd/src/rpc/mining.cpp:759-761`). Older nodes only send the latter.
    pub lightclientroothash: Option<String>,
    pub finalsaplingroothash: Option<String>,
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
    pub light_client_root: String,
    pub flags: String,
}

impl BlockTemplate {
    pub fn light_client_root(&self) -> &str {
        self.lightclientroothash
            .as_deref()
            .or(self.finalsaplingroothash.as_deref())
            .unwrap_or("")
    }

    pub fn change_key(&self) -> ChangeKey {
        ChangeKey {
            height: self.height,
            target: self.target.clone(),
            light_client_root: self.light_client_root().to_string(),
            flags: self.coinbaseaux.flags.clone(),
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
        assert_eq!(t.light_client_root(), "b95a1b9634a9157950252feeb9885ed1c9764e1741da98a6202abae39c454753");
        let k = t.change_key();
        let t4: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        assert_eq!(t4.flags_bytes().unwrap(), Vec::<u8>::new());
        assert_ne!(k, t4.change_key());
        // only the flags differ → still a new key (Y-F2)
        let mut t2 = t.clone();
        t2.coinbaseaux.flags = String::new();
        assert_ne!(t2.change_key(), k);
        // a node without -yellowback omits coinbaseaux entirely
        let mut v: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/regtest-template-4-noflags.json")).unwrap();
        v.as_object_mut().unwrap().remove("coinbaseaux");
        v.as_object_mut().unwrap().remove("lightclientroothash");
        let t: BlockTemplate = serde_json::from_value(v).unwrap();
        assert_eq!(t.coinbaseaux.flags, "");
        assert_eq!(t.light_client_root(), "a7781dd48ddbcf3dca287f0cbdcd571d9003266c40c2c6adba6605404f407e9f");
    }
}

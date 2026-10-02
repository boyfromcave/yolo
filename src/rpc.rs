// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! JSON-RPC 1.0 over HTTP to `ycashd`. The Perl shells out to `ycash-cli` (`stratumsolo:294`);
//! this talks to the node directly. Only four methods are used: `getblockchaininfo`,
//! `getblocktemplate`, `submitblock`, `validateaddress`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct RpcAuth {
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Default)]
pub struct ConfFile {
    pub rpcuser: Option<String>,
    pub rpcpassword: Option<String>,
    pub rpcport: Option<u16>,
    pub rpcbind: Option<String>,
    pub regtest: bool,
    pub testnet: bool,
    pub datadir: Option<PathBuf>,
}

impl ConfFile {
    /// `key=value` lines; `#` comments; `[section]` headers are ignored (there are none in a
    /// Ycash conf, but a stray one must not be read as a key).
    pub fn parse(text: &str) -> ConfFile {
        let mut c = ConfFile::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "rpcuser" => c.rpcuser = Some(v.to_string()),
                "rpcpassword" => c.rpcpassword = Some(v.to_string()),
                "rpcport" => c.rpcport = v.parse().ok(),
                "rpcbind" => c.rpcbind = Some(v.to_string()),
                "regtest" => c.regtest = v == "1",
                "testnet" => c.testnet = v == "1",
                "datadir" => c.datadir = Some(PathBuf::from(v)),
                _ => {}
            }
        }
        c
    }

    pub fn load(path: &Path) -> Result<ConfFile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        Ok(ConfFile::parse(&text))
    }

    /// Default RPC port for the network the conf selects (Ycash: 8832 / 18832 / 18232).
    pub fn default_rpc_port(&self) -> u16 {
        if self.regtest {
            18232
        } else if self.testnet {
            18832
        } else {
            8832
        }
    }

    pub fn url(&self) -> String {
        let host = self.rpcbind.clone().unwrap_or_else(|| "127.0.0.1".into());
        format!("http://{}:{}", host, self.rpcport.unwrap_or_else(|| self.default_rpc_port()))
    }
}

/// `.cookie` file: `__cookie__:<token>`.
pub fn read_cookie(path: &Path) -> Result<RpcAuth, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let text = text.trim();
    let (user, password) = text.split_once(':').ok_or_else(|| format!("{}: not a user:password cookie", path.display()))?;
    Ok(RpcAuth { user: user.to_string(), password: password.to_string() })
}

#[derive(Debug)]
pub enum RpcError {
    /// Transport or HTTP failure: the node is down or unreachable.
    Transport(String),
    /// The node answered with a JSON-RPC error object.
    Node { code: i64, message: String },
    /// The body was not what a JSON-RPC response looks like.
    Protocol(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(s) => write!(f, "rpc transport: {}", s),
            RpcError::Node { code, message } => write!(f, "rpc error {}: {}", code, message),
            RpcError::Protocol(s) => write!(f, "rpc protocol: {}", s),
        }
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone)]
pub struct RpcClient {
    url: String,
    authorization: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").field("url", &self.url).finish()
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

impl RpcClient {
    pub fn new(url: &str, auth: &RpcAuth) -> RpcClient {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        RpcClient {
            url: url.trim_end_matches('/').to_string(),
            authorization: format!("Basic {}", base64(format!("{}:{}", auth.user, auth.password).as_bytes())),
            agent: config.new_agent(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "1.0", "id": "yolo", "method": method, "params": params });
        let response = self
            .agent
            .post(&self.url)
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().map_err(|e| RpcError::Transport(e.to_string()))?;
        parse_response(status, &text)
    }

    pub fn getblockchaininfo(&self) -> Result<Value, RpcError> {
        self.call("getblockchaininfo", json!([]))
    }

    pub fn getblocktemplate(&self) -> Result<Value, RpcError> {
        self.call("getblocktemplate", json!([]))
    }

    /// `submitblock`: `Ok(None)` when the node accepted (JSON `null`), `Ok(Some(verdict))` for a
    /// rejection string such as `inconclusive`, `invalid-solution`, `high-hash`, `duplicate`.
    pub fn submitblock(&self, block_hex: &str) -> Result<Option<String>, RpcError> {
        match self.call("submitblock", json!([block_hex]))? {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s)),
            other => Ok(Some(other.to_string())),
        }
    }

    pub fn validateaddress(&self, address: &str) -> Result<ValidateAddress, RpcError> {
        let v = self.call("validateaddress", json!([address]))?;
        serde_json::from_value(v).map_err(|e| RpcError::Protocol(e.to_string()))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValidateAddress {
    pub isvalid: bool,
    #[serde(rename = "scriptPubKey")]
    pub script_pubkey: Option<String>,
    pub ismine: Option<bool>,
}

fn parse_response(status: u16, text: &str) -> Result<Value, RpcError> {
    let v: Value = serde_json::from_str(text).map_err(|e| {
        if status == 401 {
            RpcError::Transport("401 unauthorized: check rpcuser/rpcpassword or the cookie".into())
        } else {
            RpcError::Protocol(format!("HTTP {}: not JSON ({}): {}", status, e, text.chars().take(120).collect::<String>()))
        }
    })?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::Node {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        });
    }
    v.get("result").cloned().ok_or_else(|| RpcError::Protocol(format!("HTTP {}: no result field", status)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_parsing() {
        let c = ConfFile::parse("# comment\nregtest=1\nrpcuser=u # trailing\nrpcpassword=p=q\nrpcport=18700\n[x]\nfoo=bar\n");
        assert_eq!(c.rpcuser.as_deref(), Some("u"));
        assert_eq!(c.rpcpassword.as_deref(), Some("p=q"));
        assert_eq!(c.rpcport, Some(18700));
        assert!(c.regtest);
        assert_eq!(c.url(), "http://127.0.0.1:18700");
        let c = ConfFile::parse("testnet=1\n");
        assert_eq!(c.url(), "http://127.0.0.1:18832");
        assert_eq!(ConfFile::parse("").url(), "http://127.0.0.1:8832");
        assert_eq!(ConfFile::parse("regtest=1").url(), "http://127.0.0.1:18232");
    }

    #[test]
    fn base64_basic_auth() {
        assert_eq!(base64(b"u:p"), "dTpw");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[test]
    fn response_parsing() {
        assert_eq!(parse_response(200, r#"{"result":null,"error":null,"id":"yolo"}"#).unwrap(), Value::Null);
        assert_eq!(parse_response(200, r#"{"result":"inconclusive","error":null}"#).unwrap(), json!("inconclusive"));
        match parse_response(500, r#"{"result":null,"error":{"code":-22,"message":"Block decode failed"}}"#) {
            Err(RpcError::Node { code, message }) => {
                assert_eq!(code, -22);
                assert_eq!(message, "Block decode failed");
            }
            other => panic!("{:?}", other),
        }
        assert!(matches!(parse_response(401, ""), Err(RpcError::Transport(_))));
        assert!(matches!(parse_response(200, "<html>"), Err(RpcError::Protocol(_))));
    }
}

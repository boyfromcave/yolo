// Semgrep rule tests for rules/yolo.yaml (`semgrep --test contrib/semgrep/rules`). Not compiled.
fn rpc(client: &RpcClient) {
    // ruleid: yolo.rpc.yed-call, yolo.rpc.non-stock-method
    client.call("yed_getbalance", json!([]));
    // ruleid: yolo.rpc.non-stock-method
    client.call("sendrawtransaction", json!([]));
    // ok: yolo.rpc.non-stock-method
    client.call("getblocktemplate", json!([]));
}

async fn read(reader: &mut BufReader<OwnedReadHalf>) {
    let mut s = String::new();
    // ruleid: yolo.net.unbounded-read
    reader.read_line(&mut s).await;
    // ok: yolo.net.unbounded-read
    read_line_capped(reader, &mut buf).await;
}

fn handle(params: &[Value]) -> Result<(), String> {
    // ruleid: yolo.stratum.panic-on-miner-input
    let job = params.get(1).unwrap();
    // ok: yolo.stratum.panic-on-miner-input
    let nonce = params.get(2).and_then(Value::as_str).unwrap_or("");
    Ok(())
}

mod tests {
    fn t() {
        // ok: yolo.stratum.panic-on-miner-input
        handle(&[]).unwrap();
    }
}

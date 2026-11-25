#!/usr/bin/env python3
# Copyright (c) 2026 The Ycash developers
# Distributed under the MIT software license, see the accompanying
# file LICENSE or https://www.opensource.org/licenses/mit-license.php .
"""yolo against a Yugen-activated ycashd (regtest; TEST-ONLY stub bundle proofs).

One ycashd (Vault at 16, Yugen at 32, -yugenteststubproofs) and the release yolo binary, driven by
the Python (48,5) stratum miner:

1. yolo mines blocks 30..34, across Yugen's activation at 32: every block accepted, the tip's
   branch id becomes 59474e30 (so the coinbase it built carries the right branch id).
2. With a Yugen leaf transaction (a yewd-yugen deposit), a bundle (submitbundle; its proofs go in
   the block's sidecar) and an ordinary transaction in the template, one block each for a Sapling
   miner and a post-quantum miner (paired jobs, getcoinbasetemplatefor) and a transparent miner
   (swap path): each block holds the template's Yugen transactions, keeps the node's record run,
   and pays its miner exactly what the node's own coinbase pays the miner (vout[0] of the stock
   coinbasetxn: subsidy - fund + every fee, the Yugen transfer fees included).
3. A Yugen username (yqrf...) is refused at mining.authorize with the Yugen message.

  tests/yugen_regtest.py   (env: YELLOWBACK_WORKSPACE, YCASHD, YUGEN_LEAFVERIFY, YEWD_YUGEN, YOLO_BIN, STRATUM_MINER, PYTHON,
                            YUGEN_RT_PORT=30410)
"""
import base64, json, os, shutil, struct, subprocess, sys, tempfile, time, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLONES = os.path.dirname(ROOT)
WS = os.environ.get('YELLOWBACK_WORKSPACE', CLONES)
YCASHD = os.environ.get('YCASHD', os.path.join(CLONES, 'ycash6', 'src', 'ycashd'))
HELPER = os.environ.get('YUGEN_LEAFVERIFY', os.path.join(
    CLONES, 'ycash6', 'contrib', 'yugen-leafverify', 'target', 'release', 'yugen-leafverify'))
WALLET = os.environ.get('YEWD_YUGEN', os.path.join(CLONES, 'yew', 'yewd', 'yugen', 'target', 'release', 'yewd-yugen'))
YOLO = os.environ.get('YOLO_BIN', os.path.join(ROOT, 'target', 'release', 'yolo'))
MINER = os.environ.get('STRATUM_MINER', os.path.join(WS, 'ycash-dd', 'contrib', 'yellowback', 'devnet', 'stratum-miner'))
PYTHON = os.environ.get('PYTHON', os.path.join(WS, '.venv', 'bin', 'python'))
PORT = int(os.environ.get('YUGEN_RT_PORT', '30410'))
RPC, P2P, STRATUM, STATUS = PORT, PORT + 1, PORT + 2, PORT + 3
ACT = 32
YUGEN = 0x59474E30
ARGS = ['-regtest'] + ['-nuparams=%s:1' % b for b in ('5ba81b19', '76b809bb', '374d694f', '8e471bd6', '66314da3',
                                                      '19bd2d2f')] + [
    '-nuparams=6d5b7a31:16', '-nuparams=59474e30:%d' % ACT, '-yugenverifier=' + HELPER, '-yugenteststubproofs',
    '-pqfalcon=1', '-allowdeprecated=getnewaddress', '-allowdeprecated=z_getnewaddress', '-txindex', '-listen=0',
    '-showmetrics=0', '-experimentalfeatures']
FAIL = []


def check(cond, what):
    print(('ok    ' if cond else 'FAIL  ') + what, flush=True)
    if not cond:
        FAIL.append(what)


def rpc(method, *params):
    req = urllib.request.Request('http://127.0.0.1:%d' % RPC, json.dumps(
        {'jsonrpc': '1.0', 'id': 1, 'method': method, 'params': list(params)}).encode())
    req.add_header('Authorization', 'Basic ' + base64.b64encode(b'u:p').decode())
    try:
        r = json.loads(urllib.request.urlopen(req, timeout=600).read())
    except urllib.error.HTTPError as e:
        r = json.loads(e.read())
    if r.get('error'):
        raise RuntimeError('%s: %s' % (method, r['error']))
    return r['result']


def run(cmd, timeout=1800):
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if out.returncode != 0:
        raise RuntimeError('%s: %s %s' % (cmd[:2], out.stdout, out.stderr[-2000:]))
    return out.stdout.strip()


def compact(n):
    return bytes([n]) if n < 253 else b'\xfd' + struct.pack('<H', n) if n < 0x10000 else b'\xfe' + struct.pack('<I', n)


def bundle_tx(bundle):
    return (struct.pack('<I', 4 | 0x80000000) + struct.pack('<I', 0x59474E47) + b'\x00\x00' + b'\x00' * 8 +
            b'\x00' * 8 + b'\x00\x00\x00' + compact(len(bundle)) + bundle + b'\x00\x00').hex()


def zat(o):
    return o['valueZat'] if 'valueZat' in o else round(o['value'] * 1e8)


def mine(user, expect_ok=True):
    out = subprocess.run([PYTHON, MINER, '--pool', '127.0.0.1:%d' % STRATUM, '--user', user, '--blocks', '1',
                          '--equihash', '48,5'], capture_output=True, text=True, timeout=600)
    text = out.stdout + out.stderr
    if expect_ok:
        assert out.returncode == 0 and 'accepted job' in out.stdout, text
    return text


def send(address, amount):
    """An ordinary transaction from one coin the node's UTXO set still has. The wallet's own coin
    selection is not used: on this node it lists coins that are already spent (open item)."""
    ins, total = [], 0.0
    for u in sorted(rpc('listunspent'), key=lambda u: -u['amount']):
        if total < amount + 0.001 and rpc('gettxout', u['txid'], u['vout'], True) is not None:
            ins.append({'txid': u['txid'], 'vout': u['vout']})
            total += u['amount']
    if total < amount + 0.001:
        raise RuntimeError('not enough unspent coins')
    raw = rpc('createrawtransaction', ins, {address: amount, rpc('getnewaddress'): round(total - amount - 0.0001, 8)})
    return rpc('sendrawtransaction', rpc('signrawtransaction', raw)['hex'])


def main():
    work = tempfile.mkdtemp(prefix='yugen-yolo-rt-')
    ndir = os.path.join(work, 'node')
    os.makedirs(ndir)
    with open(os.path.join(ndir, 'ycash.conf'), 'w') as f:
        f.write('regtest=1\nrpcuser=u\nrpcpassword=p\nrpcport=%d\nport=%d\n' % (RPC, P2P))

    def start(extra=()):
        n = subprocess.Popen([YCASHD, '-datadir=' + ndir] + ARGS + list(extra), stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL)
        for _ in range(120):
            try:
                rpc('getblockcount')
                return n
            except Exception:
                time.sleep(1)
        raise RuntimeError('ycashd did not start')

    node, pool = start(), None
    try:
        mineraddress = rpc('getnewaddress')
        rpc('stop')
        node.wait(timeout=120)
        node = start(['-mineraddress=' + mineraddress])
        rpc('generate', 29)
        pool = subprocess.Popen([YOLO, '--rpc', 'http://127.0.0.1:%d' % RPC, '--rpc-user', 'u', '--rpc-password',
                                 'p', '--port', str(STRATUM), '--bind', '127.0.0.1', '--status-port', str(STATUS),
                                 '--equihash', '48,5'], stdout=open(os.path.join(work, 'yolo.log'), 'w'),
                                stderr=subprocess.STDOUT)
        time.sleep(3)
        t_addr = rpc('getnewaddress')
        # 1. across the activation
        for _ in range(5):
            mine(t_addr)
        info = rpc('getblockchaininfo')
        check(info['blocks'] == 34 and info['consensus']['chaintip'] == '59474e30',
              'yolo mined 30..34 across Yugen at %d; tip branch %s' % (ACT, info['consensus']['chaintip']))

        rpc('generate', 106)
        reg = run([HELPER, 'regroot'])
        wdir = os.path.join(work, 'wallet')
        os.makedirs(wdir)
        wallet = lambda *a: json.loads(run([WALLET, '-network=regtest', '-node=127.0.0.1:%d' % RPC, '-rpcuser=u',
                                            '-rpcpassword=p', '-datadir=' + wdir, '-seed=' + '33' * 32] + list(a)))
        dep = wallet('depositaddress')
        dep = dep if isinstance(dep, str) else dep['address']
        fund = send(dep, 20)
        rpc('generate', 1)
        payee = rpc('validateaddress', rpc('getnewaddress'))['scriptPubKey']
        z_addr = rpc('z_getnewaddress', 'sapling')
        pq_addr = rpc('getnewaddress', 'pq')
        miners = [('sapling', z_addr, None), ('pq', pq_addr, rpc('validateaddress', pq_addr)['scriptPubKey']),
                  ('p2pkh', t_addr, rpc('validateaddress', t_addr)['scriptPubKey'])]

        for i, (kind, user, script) in enumerate(miners):
            yugen = []
            plain = send(rpc('getnewaddress'), 1)
            if i == 0:   # the leaf: the deposit (shield)
                before = set(rpc('getrawmempool'))
                wallet('shield', json.dumps([fund]))
                yugen += [t for t in rpc('getrawmempool') if t not in before]
            else:        # a bundle (the pool is funded by the deposit mined in the previous block)
                h = rpc('getblockcount') + 1
                ha = (h - 1) // 16 * 16
                a = rpc('yugen_getanchor', ha)
                req = {'anchor': {'slot': (ha // 16) & 0xff, 'anchor_root': a['root'], 'reg_root': reg,
                                  'block_hash': a['block_hash']}, 'n': 3, 'seed': i, 'payee': payee,
                       'branch_id': YUGEN, 'network': 2, 'height': h, 'activation': ACT}
                path = os.path.join(work, 'tb.json')
                with open(path, 'w') as f:
                    json.dump(req, f)
                b, pa, pb = run([HELPER, 'testbundle', path]).split()
                yugen.append(rpc('submitbundle', {'tx': bundle_tx(bytes.fromhex(b)), 'proofs': [pa, pb]})['txid'])
            for _ in range(60):   # the node renews a template for the same tip after 5 s of mempool change
                t = rpc('getblocktemplate')
                ids = [x['hash'] for x in t['transactions']]
                if all(y in ids for y in yugen + [plain]):
                    break
                time.sleep(1)
            time.sleep(6)   # yolo's next refresh carries the same template
            check(len(yugen) == 1, '%s: one Yugen tx submitted' % kind)
            check(all(y in ids for y in yugen) and plain in ids,
                  '%s: template holds the Yugen tx(s) and the ordinary tx' % kind)
            stock = rpc('decoderawtransaction', t['coinbasetxn']['data'])
            want, tail = zat(stock['vout'][0]), [(zat(o), o['scriptPubKey']['hex']) for o in stock['vout'][1:]]
            mine(user)
            blk = rpc('getblock', rpc('getbestblockhash'), 2)
            txids = [x['txid'] for x in blk['tx']]
            check(blk['height'] == t['height'] and all(y in txids for y in yugen) and plain in txids,
                  '%s: block %d mined by yolo with the Yugen tx(s)' % (kind, blk['height']))
            cb = blk['tx'][0]
            outs = [(zat(o), o['scriptPubKey']['hex']) for o in cb['vout']]
            if kind == 'sapling':
                paid = -cb.get('valueBalanceZat', round(cb['valueBalance'] * 1e8))
                rest = outs
            else:
                mine_out = [o for o in outs if o[1] == script]
                paid = mine_out[0][0] if len(mine_out) == 1 else -1
                rest = [o for o in outs if o[1] != script]
            check(paid == want, '%s: miner paid %d zat, the node\'s coinbase pays %d' % (kind, paid, want))
            check(rest == tail, '%s: the record run / fund outputs are the stock coinbase\'s (%d)' % (kind, len(tail)))

        # 4. a transaction arriving mid-job is mined in the next block: yolo already has work for
        # this height; the test never asks for a template itself, so only yolo's own poll can pick it up.
        import socket
        sock = socket.create_connection(('127.0.0.1', STRATUM), timeout=2)
        for m in ({'id': 1, 'method': 'mining.subscribe', 'params': []},
                  {'id': 2, 'method': 'mining.authorize', 'params': [t_addr, 'x']}):
            sock.sendall((json.dumps(m) + '\n').encode())
        def notifies(seconds):
            buf, n, end = b'', 0, time.time() + seconds
            while time.time() < end:
                try:
                    chunk = sock.recv(65536)
                except socket.timeout:
                    continue
                if not chunk:
                    break
                buf += chunk
            return sum(1 for l in buf.splitlines() if b'mining.notify' in l)
        check(notifies(3) >= 1, 'a connected miner has work')
        late = send(rpc('getnewaddress'), 1)
        check(notifies(8) >= 1, 'a transaction arriving mid-job sends the connected miner new work')
        sock.close()
        mine(t_addr)
        blk = rpc('getblock', rpc('getbestblockhash'))
        check(late in blk['tx'], 'a transaction sent mid-job is mined in the next block (%d)' % blk['height'])

        # 3. a Yugen username
        text = mine('yqrf1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq', expect_ok=False)
        log = open(os.path.join(work, 'yolo.log')).read()
        check('Yugen address' in log and 'accepted job' not in text, 'yqrf username refused with the Yugen message')
    finally:
        if pool:
            pool.terminate()
            pool.wait()
        try:
            rpc('stop')
        except Exception:
            node.terminate()
        node.wait(timeout=120)
        if FAIL:
            print('kept', work)
        else:
            shutil.rmtree(work, ignore_errors=True)
    print('%d failed' % len(FAIL))
    return 1 if FAIL else 0


if __name__ == '__main__':
    sys.exit(main())

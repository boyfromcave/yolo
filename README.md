# Yolo : Ycash Solo-Mining Stratum Servers

> **Rust rewrite in progress.** The `yolo` binary (`cargo build --release`, `src/`) replaces the
> three Perl scripts below with one program (`yolo --mode solo|pool|cenote`) that talks JSON-RPC
> to the node directly, keeps the Perl's stratum wire format, and carries the Ycash Yellowback
> (YED) coinbase tag on every mode. See `docs/plans/role-pool-regtest-plan.md` in the workspace.
> The Perl files stay here until parity is declared; this README still describes them.

These are a very simple stratum servers for connecting GPU miners to a Ycash full node.

- StratumPool : Clients can specify their own transparent ycash address.
- StratumSolo : All clients mine to the local node wallet.

Both operate as 'solo-pools', miners ONLY get paid if they find a block.

Payments are made within the coinbase transaction, which means :-
- Rewards must mature for 100 blocks before they can be spent.
- The first transaction MUST be to a shielded address (s2y).

There's no dev fee but don't expect any support !! 

If you'd like to buy beers for the original developer (@ChileBob) :-
- YCASH : ys1c8cvazsz5gfp2zhdmzxcarfh4gp6jezdcxnywfcpyuau0l0f9uj99tzvrr6sjw5rfhpsw06lc6n
- ZCASH : zs18zekcwmw9murkl0qazjz5hz4uenaf9pyw53as6f4plx39m92elfyhljgxwc25t3s5uerzqtmf0w

## Requirements :-

- Ycash full node running on Ubuntu/Debian Linux
- GPU Hardware (ie: Nvidia, AMD)
- GPU Mining Software for Equihash 192,7 (ie: miniZ v1.6w)

## Tested Miners :-

- GMiner (v2.28)
- lolMiner (v0.8.4)
- miniZ (v1.6w)

## Usage :-

stratumpool 

* `--port=3333 (port for miner connections, default 3333)`
* `--password=notsecret (optional password miners need to connect, default not set)`

stratumsolo (no config needed)

* default port 3334

## Installation  :-

* sudo apt-get update
* sudo apt-get upgrade
* sudo apt-get install perl build-essential cpan
* sudo cpan
* install CPAN
* reload
* exit

* sudo cpan install POSIX IO::Handle IO::Socket IO::Select JSON	Digest::SHA


## Important !!! Don't forget !!!

You MUST add a line to your node configuration file (ycash.conf) to set a fixed mining address, ie :-

mineraddress=s1youraddressgoeshere

* This MUST be a transparent address (starts with 's1')
* The address MUST belong to the node wallet (use 'ycash-cli getnewaddress')

Before you can spend any coins you have mined, you must :-

* Wait 100 blocks for the reward to mature.
* Send the ENTIRE balance of the saddr to a yaddr you control. (Use YECwallet FullNode)


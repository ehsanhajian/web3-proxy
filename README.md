# web3_proxy

Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.

**Under construction!** This code is under active development. If you want to run this proxy youself, send me a message on [Twitter](https://twitter.com/StittsHappening) and I can explain things that aren't documented yet. Most RPC methods are supported, but filters are coming soon. And of course, more tests are always needed.

Signed transactions (eth_sendRawTransaction) are sent in parallel to the configured private RPCs (eden, ethermine, flashbots, etc.).

All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers). If multiple servers are in sync, they are prioritized by `active_requests/soft_limit`. Note that this means that the fastest server is most likely to serve requests and slow servers are unlikely to ever get any requests.

Each server has different limits to configure. The `soft_limit` is the number of parallel active requests where a server starts to slow down. The `hard_limit` is where a server starts giving rate limits or other errors.

```
$ cargo run --release -- --help
```
```
   Compiling web3_proxy v0.1.0 (/home/bryan/src/web3_proxy/web3_proxy)
    Finished release [optimized + debuginfo] target(s) in 17.69s
     Running `target/release/web3_proxy --help`
Usage: web3_proxy [--port <port>] [--workers <workers>] [--config <config>]

web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.

Options:
  --port            what port the proxy should listen on
  --workers         number of worker threads
  --config          path to a toml of rpc servers
  --help            display usage information
```

Start the server with the defaults (listen on `http://localhost:8544` and use `./config/example.toml` which proxies to a bunch of public nodes:

```
cargo run --release -- --config ./config/example.toml
```

## Common commands

Create a user:

```
cargo run --bin web3_proxy_cli -- --db-url "$YOUR_DB_URL" create_user --address "$USER_ADDRESS_0x"
```

Check that the proxy is working:

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"web3_clientVersion","id":1}' 127.0.0.1:8544
```

Check that the websocket is working:

```
$ websocat ws://127.0.0.1:8544

{"id": 1, "method": "eth_subscribe", "params": ["newHeads"]}

{"id": 2, "method": "eth_subscribe", "params": ["newPendingTransactions"]}

{"id": 3, "method": "eth_subscribe", "params": ["newPendingFullTransactions"]}

{"id": 4, "method": "eth_subscribe", "params": ["newPendingRawTransactions"]}
```

You can copy `config/example.toml` to `config/production-$CHAINNAME.toml` and then run `docker-compose up --build -d` start proxies for many chains.

## Database entities

This command only needs to be run during development. Production should use the already generated entities.

When developing new database migrations, **after you migrate**, run this command to generate updated entity files. It's best to keep the migration and entity changes in the same commit.

```
cargo install sea-orm-cli
```
```
sea-orm-cli generate entity -u mysql://root:dev_web3_proxy@127.0.0.1:13306/dev_web3_proxy -o entities/src --with-serde both
```

After running the above, you will need to manually fix some columns: `Vec<u8>` -> `sea_orm::prelude::Uuid` and `i8` -> `bool`. Related: <https://github.com/SeaQL/sea-query/issues/375> <https://github.com/SeaQL/sea-orm/issues/924>

## Flame Graphs

Flame graphs make a developer's join of finding slow code painless:

    $ cat /proc/sys/kernel/kptr_restrict
    1
    $ echo 0 | sudo tee /proc/sys/kernel/kptr_restrict
    0
    $ CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph


## GDB

Developers can run the proxy under gdb for advanced debugging:

    cargo build --release && RUST_LOG=web3_proxy=debug rust-gdb --args target/debug/web3_proxy --listen-port 7503 --rpc-config-path ./config/production-eth.toml

TODO: also enable debug symbols in the release build by modifying the root Cargo.toml

## Load Testing

Test the proxy:

    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544/u/$API_KEY
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8544/u/$API_KEY

Test geth (assuming it is on 8545):

    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8545

Test erigon (assuming it is on 8945):

    wrk -s ./wrk/getBlockNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945
    wrk -s ./wrk/getLatestBlockByNumber.lua -t12 -c400 -d30s --latency http://127.0.0.1:8945

Note: Testing with `getLatestBlockByNumber.lua` is not great because the latest block changes and so one run is likely to be very different than another.

Run [ethspam](https://github.com/INFURA/versus) and [versus](https://github.com/shazow/ethspam) for a more realistic load test:

    ethspam --rpc http://127.0.0.1:8544/u/$API_KEY | versus --concurrency=100 --stop-after=10000 http://127.0.0.1:8544/u/$API_KEY

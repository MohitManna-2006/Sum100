# Kalshi live fixtures

All venue payload files here are captured production bytes for
`KXBTCD-26SEP1417-T76999.99`. They are not JSON re-serializations. Each record
contains the raw WebSocket text with its trailing LF removed and exactly one
file delimiter LF added. Original key order, decimal strings, and whitespace
are preserved. No payload was synthesized.

- `stage1-orderbook.ndjson`: recovered original stage 1 capture on September 13,
  2026, around 23:28:35 UTC. One subscription acknowledgement, the **complete**
  88-level snapshot previously displayed in truncated form, and all 156
  consecutive deltas (seq 2–157). Sides: 102 yes, 54 no. The stage 1 terminal
  sample showed only yes, but the saved session contained both sides.
- `stage1-ticker.ndjson`: recovered original ticker/trade subscription session
  around 23:27 UTC. Two subscription acknowledgements and 31 ticker messages;
  no trade message arrived. For both original files, only the extra blank
  separator lines introduced by the old probe's `println!` were removed.
- `stage2-live-orderbook.ndjson`: fresh 70-second stage 2 recording started
  September 13, 2026 at 23:45:20 UTC using the new recorder. One acknowledgement,
  one complete snapshot at seq 1, and 460 consecutive deltas at seq 2–461.
  Sides: 251 yes, 209 no. Every raw text message from this session is included.
- `no-side-delta.json`: byte-for-byte copy of seq 4 from the fresh session.
  Wire side is `no`, price is `0.5500`, delta is `-1000.00`. The feed must retain
  price 55; phase 2 will own its complement conversion. This message is also
  present in the full sequence, so it can be replayed in context.

Fresh recording command:

```sh
cargo run --release -- record --venue kalshi --prod \
  --tickers KXBTCD-26SEP1417-T76999.99 \
  --out data/stage2-validation --seconds 70
```

The source gzip has 462 envelopes and is 17,982 bytes. The first-to-last local
receipt span is 69,646 ms. It grew during the run and read back completely with
no empty records and strictly consecutive recorder sequences. The extraction
decoded each envelope's `raw` string and wrote those bytes directly; it did not
parse and re-serialize the venue payload. Source gzip SHA-256:
`cc644cf5c8d34c31205f8ca2d714b5de3f1e411d32eb5fe03ff5ef35aacc5500`.

Payload SHA-256 values:

| File | SHA-256 |
| --- | --- |
| stage1-orderbook.ndjson | `5891d7ef28aa44b8277eacbc06242ac86f6173bb195452b07fee493905738e90` |
| stage1-ticker.ndjson | `43b83a61a56f6aaa97566229c56f3b02e564bc9ed86e55b143311f38c97bae1c` |
| stage2-live-orderbook.ndjson | `0887f5030dfb03e35a0e930f76945b163cce2ccdd808f9ee5828e2459083df8c` |
| no-side-delta.json | `de4efc7d3f6908347be654e9b75e91ed2479b876fc88f1e2c63325c20072ca2a` |

The corpus files stay on disk so phase 3 can reuse them. Test-only malformed
payloads are deliberate mutations, clearly separated from captured fixtures.
No credentials or private API payloads are included.

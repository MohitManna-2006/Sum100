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
  price 55; the book store converts it to a yes ask at 45. This message is also
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

## Phase 3 disconnect session

- `phase3-disconnect-session.ndjson.gz`: the unmodified daily file written by a
  production `dump` of `KXBTCD-26SEP1417-T76999.99`, `-T77499.99`, `-T77749.99`,
  and `-T77999.99`, started September 14, 2026 at 03:19:21 UTC. The process was
  suspended with SIGSTOP from 03:20:01 to 03:28:01 UTC; on resume the feed's
  45-second idle watchdog dropped the websocket, reconnected, resubscribed, and
  received four fresh snapshots. 2,282 text envelopes and 4 control envelopes
  (`session_started`, `disconnected`, `reconnected`, `resubscribed`).
- `phase3-disconnect-session.digest`: the state digest that live run wrote with
  `--digest-out` before replay existed for it. `recorded_disconnect_session_replays_to_live_digest`
  requires replay to render it byte for byte.

| File | SHA-256 |
| --- | --- |
| phase3-disconnect-session.ndjson.gz | `7d092c75fc4dad8fd530e61d3c23bf1323c74268b4419dff824e554d936ee97e` |
| phase3-disconnect-session.digest | `e4309003f6635844e53970732fa585ce8edecd93c2b1bf3e3194cba39bf2eb9f` |

Evidence and commands: [docs/phase-3-summary.md](../../docs/phase-3-summary.md).

## Phase 2.1 one-sided snapshots

- `phase3-d-one-sided-snapshots.ndjson.gz`: 155 envelope lines copied byte for
  byte from `data/phase3-d/production/kalshi-2026-09-14.ndjson.gz` (source SHA-256
  `74cfa726635cdc1ce65cd0868b5372ad135129c717567033f89b87b08fad0d48`): recorder
  sequence 1 (`session_started`, 80 `KXBTCD-26SEP1417` strikes), then the text
  envelopes of one connection, sequence 16878–17032 (ack, 80 snapshots at venue seq
  1–80, 73 deltas at seq 81–153, received 03:25:55.909–03:25:56.435 UTC). That
  connection's `reconnected` (16877), `resubscribed` (16879), and `disconnected`
  (17033) control envelopes are omitted so the slice replays as one fresh session.
  31 snapshots omit `no_dollars_fp` and 20 omit `yes_dollars_fp`; 70 deltas are
  on those one-sided strikes. Lines were not re-serialized; the gzip is written with
  mtime 0. Live, this connection hit a false gap and a forced reconnect.

| File | SHA-256 |
| --- | --- |
| phase3-d-one-sided-snapshots.ndjson.gz | `0501b38367cd67c7d0d22f02569b3f65028c8799f8312b7e5a73d5edaa2fee7c` |

The corpus files stay on disk so later phases can reuse them. Test-only malformed
payloads are deliberate mutations, clearly separated from captured fixtures.
No credentials or private API payloads are included.

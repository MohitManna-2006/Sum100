# Sum100 frontend

Dark, information-dense dashboard for Sum100's prediction-market coherence and arbitrage engine. Opportunities and health consume the live engine WebSocket; coherence remains a labeled preview until the backend publishes its coherence state contract.

## Run locally

```sh
npm install
npm run dev
```

The development server is available at `http://localhost:5173`.
Set `VITE_API_URL` in `.env.local` when the engine API is not running at
`http://localhost:8080`.

Start the production read-only engine and dashboard WebSocket from the
repository root:

```sh
cargo run --release -- scan --live --prod --registry config/registry.live.toml
```

This serves `ws://localhost:8080/ws`. Kalshi credentials must be configured in
`KALSHI_KEY_ID` and `KALSHI_PRIVATE_KEY_PATH`.

## Checks

```sh
npm run lint
npm run build
npm test
```

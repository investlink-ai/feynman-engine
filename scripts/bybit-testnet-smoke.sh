#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${BYBIT_API_KEY:-}" || -z "${BYBIT_API_SECRET:-}" ]]; then
  cat <<'EOF'
BYBIT_API_KEY and BYBIT_API_SECRET must be set.

This project reads Bybit credentials from config/default.toml via:
  [venues.bybit]
  api_key = "${BYBIT_API_KEY}"
  api_secret = "${BYBIT_API_SECRET}"

Example:
  export BYBIT_API_KEY="..."
  export BYBIT_API_SECRET="..."
  ./scripts/bybit-testnet-smoke.sh

Optional:
  BYBIT_TESTNET_MARKET=BTCUSDT
  BYBIT_TESTNET_QTY=0.001
  BYBIT_TESTNET_LIMIT_OFFSET_BPS=500
  BYBIT_TESTNET_RUN_MARKET_FILL=1
EOF
  exit 1
fi

export BYBIT_TESTNET_MARKET="${BYBIT_TESTNET_MARKET:-BTCUSDT}"
export BYBIT_TESTNET_QTY="${BYBIT_TESTNET_QTY:-0.001}"
export BYBIT_TESTNET_LIMIT_OFFSET_BPS="${BYBIT_TESTNET_LIMIT_OFFSET_BPS:-500}"
export BYBIT_TESTNET_RUN_MARKET_FILL="${BYBIT_TESTNET_RUN_MARKET_FILL:-0}"

echo "running Bybit testnet smoke against ${BYBIT_TESTNET_MARKET} qty=${BYBIT_TESTNET_QTY} market_fill=${BYBIT_TESTNET_RUN_MARKET_FILL}"
cargo run -p feynman-engine --bin bybit-testnet-smoke

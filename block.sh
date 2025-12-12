#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   ./block.sh [block_time_seconds]
#
# Env vars:
#   EXEC_RPC_ENDPOINT          - Execution JSON-RPC (eth_*, default: http://localhost:8545)
#   ENGINE_RPC_ENDPOINT        - Engine API JSON-RPC (engine_*, default: http://localhost:8551)
#   JWT_FILE                   - Path to JWT secret file for Engine API (default: ./jwt.hex)
#   PARENT_BEACON_BLOCK_ROOT   - Optional 32-byte root for parentBeaconBlockRoot
#   GAS_LIMIT_OVERRIDE         - Optional hex gasLimit (e.g. 0x1c9c380), overrides parent block gasLimit
#
# Args:
#   block_time_seconds         - seconds to add to latest block timestamp (default: 2).

BLOCK_TIME_SECONDS="${1:-2}"

EXEC_RPC_ENDPOINT="${EXEC_RPC_ENDPOINT:-http://127.0.0.1:8545}"
ENGINE_RPC_ENDPOINT="${ENGINE_RPC_ENDPOINT:-http://127.0.0.1:8551}"
JWT_FILE="${JWT_FILE:-jwt.hex}"

if [[ ! -f "$JWT_FILE" ]]; then
  echo "ERROR: JWT file not found: $JWT_FILE" >&2
  exit 1
fi

# Read raw 32-byte hex (no 0x)
JWT_SECRET="$(tr -d ' \t\r\n' < "$JWT_FILE")"
if [[ -z "$JWT_SECRET" ]]; then
  echo "ERROR: JWT secret is empty" >&2
  exit 1
fi

PARENT_BEACON_BLOCK_ROOT="${PARENT_BEACON_BLOCK_ROOT:-0x0000000000000000000000000000000000000000000000000000000000000000}"

echo "Using EXEC_RPC_ENDPOINT:      $EXEC_RPC_ENDPOINT"
echo "Using ENGINE_RPC_ENDPOINT:    $ENGINE_RPC_ENDPOINT"
echo "Using JWT file:               $JWT_FILE"
echo "Using block time delta:       ${BLOCK_TIME_SECONDS}s"
echo "Using parentBeaconBlockRoot:  $PARENT_BEACON_BLOCK_ROOT"
echo

# ------------------------------
# 1) Fetch latest block from normal RPC (8545)
# ------------------------------
BLOCK_JSON="$(
  cast rpc \
    --rpc-url "$EXEC_RPC_ENDPOINT" \
    eth_getBlockByNumber latest false
)"

HEAD_HASH="$(jq -r '.hash'      <<<"$BLOCK_JSON")"
TIMESTAMP_HEX="$(jq -r '.timestamp' <<<"$BLOCK_JSON")"
MIX_HASH="$(jq -r '.mixHash // .prevRandao' <<<"$BLOCK_JSON")"
COINBASE="$(jq -r '.miner // .author' <<<"$BLOCK_JSON")"
PARENT_GAS_LIMIT="$(jq -r '.gasLimit' <<<"$BLOCK_JSON")"

if [[ "$HEAD_HASH" == "null" ]]; then
  echo "ERROR: Could not fetch head block" >&2
  exit 1
fi
if [[ "$TIMESTAMP_HEX" == "null" ]]; then
  echo "ERROR: Could not fetch head timestamp" >&2
  exit 1
fi
if [[ "$PARENT_GAS_LIMIT" == "null" || -z "$PARENT_GAS_LIMIT" ]]; then
  echo "ERROR: Could not fetch parent gasLimit" >&2
  exit 1
fi

# Use override gas limit if provided, otherwise inherit from parent block
GAS_LIMIT="${GAS_LIMIT_OVERRIDE:-$PARENT_GAS_LIMIT}"

# ------------------------------
# 2) Compute new timestamp
# ------------------------------
TIMESTAMP_DEC=$((TIMESTAMP_HEX))   # bash understands 0x... here
NEW_TS_DEC=$((TIMESTAMP_DEC + BLOCK_TIME_SECONDS))
NEW_TS_HEX="$(printf '0x%x' "$NEW_TS_DEC")"

# ------------------------------
# 3) Build forkchoice state
# ------------------------------
FORKCHOICE_STATE="$(
  jq -n --arg head "$HEAD_HASH" \
        '{headBlockHash:$head, safeBlockHash:$head, finalizedBlockHash:$head}'
)"

WITHDRAWALS_JSON="[]"

# ------------------------------
# 4) Build PayloadAttributesV3 (with gasLimit)
# ------------------------------
PAYLOAD_ATTRS="$(
  jq -n \
    --arg ts  "$NEW_TS_HEX" \
    --arg rand "$MIX_HASH" \
    --arg fee "$COINBASE" \
    --arg pbr "$PARENT_BEACON_BLOCK_ROOT" \
    --arg gas "$GAS_LIMIT" \
    --argjson withdrawals "$WITHDRAWALS_JSON" \
    '{
      timestamp: $ts,
      prevRandao: $rand,
      suggestedFeeRecipient: $fee,
      withdrawals: $withdrawals,
      parentBeaconBlockRoot: $pbr,
      gasLimit: $gas
    }'
)"

echo "forkchoiceState:"
echo "$FORKCHOICE_STATE"
echo
echo "payloadAttributes:"
echo "$PAYLOAD_ATTRS"
echo

# ------------------------------
# 5) Send engine_forkchoiceUpdatedV3 to Engine API (8551) with JWT
# ------------------------------
echo "Calling engine_forkchoiceUpdatedV3 via Engine API..."

cast rpc \
  --rpc-url "$ENGINE_RPC_ENDPOINT" \
  --jwt-secret "$JWT_SECRET" \
  engine_forkchoiceUpdatedV3 \
  "$FORKCHOICE_STATE" \
  "$PAYLOAD_ATTRS"

echo "Done."
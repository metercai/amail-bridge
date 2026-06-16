#!/usr/bin/env bash
set -eu

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
fail() { echo -e "${RED}[FAIL]${NC} $*"; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
GW_BIN="${GW_BIN:-$HOME/amail-gateway/target/debug/amail-gateway}"
BRIDGE_BIN="${BRIDGE_BIN:-$PROJECT_DIR/target/debug/amail-bridge}"
MOCK_HERMES="$SCRIPT_DIR/mock_hermes.py"
WORK_DIR="${WORK_DIR:-/tmp/bridge-e2e}"

RS=35010; RH=39010; RS2=35011; RH2=39011
BRIDGE_PORT=38080; BRIDGE_PORT2=38081; HERMES_PORT=39999

cleanup() {
    for pid in "${RELAY_PID:-}" "${BRIDGE_PID:-}" "${HERMES_PID:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT
rm -rf "$WORK_DIR"; mkdir -p "$WORK_DIR/bridge" "$WORK_DIR/relay-data"

# ════ 1. Relay (push) ════
echo ""; echo "=== 1. Push Relay ==="
cat > "$WORK_DIR/relay.toml" << EOF
[smtp]; addr = "127.0.0.1:${RS}"
[http]; addr = "127.0.0.1:${RH}"
[storage]; path = "${WORK_DIR}/relay-data"
[retry]; max_attempts = 1; initial_backoff_secs = 1
[logging]; level = "info"
EOF
"$GW_BIN" -c "$WORK_DIR/relay.toml" --pid-file "$WORK_DIR/relay.pid" > "$WORK_DIR/relay.log" 2>&1 &
RELAY_PID=$!; sleep 3
AK=$(cat "$WORK_DIR/relay-data/amail.db.admin_key" 2>/dev/null || echo "")
[[ -n "$AK" ]] || fail "no admin key"
for i in $(seq 1 15); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${RH}/health")" == "200" ]] && break
    sleep 1
done
pass "push relay running"

# ════ 2. Seed ════
echo ""; echo "=== 2. Seed ==="
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/admin/systems/admin/domains" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d "{\"id\":\"bdom\",\"domain\":\"bridge.test\",\"webhook_url\":\"http://127.0.0.1:${BRIDGE_PORT}/webhooks/bridge\"}" > /dev/null
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/admin/systems/admin/addresses" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d "{\"id\":\"bagent\",\"email\":\"agent@bridge.test\",\"webhook_url\":\"http://127.0.0.1:${BRIDGE_PORT}/webhooks/bridge\"}" > /dev/null
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/admin/whitelists" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d '{"system_id":"admin","domain_addr":"bridge.test","direction":"from","value":"*@test.local"}' > /dev/null
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/admin/systems/admin/domains" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d '{"id":"sdom","domain":"test.local"}' > /dev/null
SK=$(curl -s -X POST "http://127.0.0.1:${RH}/api/v1/api-keys" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d '{"system_id":"admin","email_address":"sender@test.local","scopes":["send","agent"],"category":"agent"}' \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('raw_key',''))" 2>/dev/null || echo "")
[[ -n "$SK" ]] || fail "no send key"
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/admin/whitelists" \
    -H "X-Api-Key: ${AK}" -H "Content-Type: application/json" \
    -d '{"system_id":"admin","domain_addr":"test.local","direction":"to","value":"*@bridge.test"}' > /dev/null
pass "seeded"

# ════ 3. Mock Hermes ════
echo ""; echo "=== 3. Mock Hermes ==="
HL="$WORK_DIR/hermes.log"; rm -f "$HL"
python3 "$MOCK_HERMES" "127.0.0.1" "$HERMES_PORT" "$HL" &
HERMES_PID=$!; sleep 1
curl -s "http://127.0.0.1:${HERMES_PORT}/health" > /dev/null || fail "hermes not healthy"
pass "mock hermes running"

# ════ 4. Routes ════
cat > "$WORK_DIR/bridge/amail_routes.toml" << EOF
"agent@bridge.test" = "127.0.0.1:${HERMES_PORT}"
"agent@pull.test" = "127.0.0.1:${HERMES_PORT}"
EOF

# ════ 5a. Push ════
echo ""; echo "=== 5a. Push Mode ==="
cat > "$WORK_DIR/bridge/bridge-push.toml" << EOF
addr = "127.0.0.1:${BRIDGE_PORT}"
routes_file = "$WORK_DIR/bridge/amail_routes.toml"
mode = "push"
[push]; body_limit_mb = 10
[logging]; level = "info"; file = "$WORK_DIR/bridge/push-bridge.log"
EOF
"$BRIDGE_BIN" -c "$WORK_DIR/bridge/bridge-push.toml" > "$WORK_DIR/bridge/push.log" 2>&1 &
BRIDGE_PID=$!; sleep 2
for i in $(seq 1 10); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${BRIDGE_PORT}/health" 2>/dev/null)" == "200" ]] && break
    sleep 1
done
pass "bridge push running"

rm -f "$HL"
curl -s -X POST "http://127.0.0.1:${RH}/api/v1/send" \
    -H "X-Api-Key: ${SK}" -H "Content-Type: application/json" \
    -d '{"sender":"sender@test.local","to":"agent@bridge.test","subject":"Push Test","markdown":"Push."}' > /dev/null
sleep 3
n=0; for i in $(seq 1 10); do n=$(wc -l < "$HL" 2>/dev/null || echo 0); [[ $n -ge 1 ]] && break; sleep 2; done
[[ $n -ge 1 ]] && pass "5a: push OK ($n)" || fail "5a: push FAIL"

kill "$BRIDGE_PID" 2>/dev/null; wait "$BRIDGE_PID" 2>/dev/null || true

# ════ 5b. Pull ════
echo ""; echo "=== 5b. Pull Mode ==="
kill "$RELAY_PID" 2>/dev/null; wait "$RELAY_PID" 2>/dev/null || true; sleep 2
rm -rf "$WORK_DIR/relay-data"; mkdir -p "$WORK_DIR/relay-data"

cat > "$WORK_DIR/relay.toml" << EOF
[smtp]; addr = "127.0.0.1:${RS2}"
[http]; addr = "127.0.0.1:${RH2}"
[storage]; path = "${WORK_DIR}/relay-data"
[retry]; max_attempts = 1; initial_backoff_secs = 1
[logging]; level = "info"
EOF
"$GW_BIN" -c "$WORK_DIR/relay.toml" --pid-file "$WORK_DIR/relay.pid" > "$WORK_DIR/relay2.log" 2>&1 &
RELAY_PID=$!; sleep 3
AK2=$(cat "$WORK_DIR/relay-data/amail.db.admin_key" 2>/dev/null || echo "")
for i in $(seq 1 15); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${RH2}/health" 2>/dev/null)" == "200" ]] && break
    sleep 1
done
pass "pull relay running"

# Seed pull agent (no webhook_url)
curl -s -X POST "http://127.0.0.1:${RH2}/api/v1/admin/systems/admin/domains" \
    -H "X-Api-Key: ${AK2}" -H "Content-Type: application/json" \
    -d '{"id":"pdom","domain":"pull.test"}' > /dev/null
curl -s -X POST "http://127.0.0.1:${RH2}/api/v1/admin/systems/admin/addresses" \
    -H "X-Api-Key: ${AK2}" -H "Content-Type: application/json" \
    -d '{"id":"pagent","email":"agent@pull.test"}' > /dev/null
curl -s -X POST "http://127.0.0.1:${RH2}/api/v1/admin/whitelists" \
    -H "X-Api-Key: ${AK2}" -H "Content-Type: application/json" \
    -d '{"system_id":"admin","domain_addr":"pull.test","direction":"from","value":"*@test.local"}' > /dev/null
SK2=$(curl -s -X POST "http://127.0.0.1:${RH2}/api/v1/api-keys" \
    -H "X-Api-Key: ${AK2}" -H "Content-Type: application/json" \
    -d '{"system_id":"admin","email_address":"sender@test.local","scopes":["send","agent"],"category":"agent"}' \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('raw_key',''))" 2>/dev/null || echo "")

# Pull bridge
cat > "$WORK_DIR/bridge/bridge-pull.toml" << EOF
addr = "127.0.0.1:${BRIDGE_PORT2}"
routes_file = "$WORK_DIR/bridge/amail_routes.toml"
mode = "pull"
[pull]; amail_url = "http://127.0.0.1:${RH2}"; admin_key = "${AK2}"; system_id = "admin"; poll_interval_sec = 2
[logging]; level = "info"; file = "$WORK_DIR/bridge/pull-bridge.log"
EOF
"$BRIDGE_BIN" -c "$WORK_DIR/bridge/bridge-pull.toml" > "$WORK_DIR/bridge/pull.log" 2>&1 &
BRIDGE_PID=$!; sleep 2
for i in $(seq 1 10); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${BRIDGE_PORT2}/health" 2>/dev/null)" == "200" ]] && break
    sleep 1
done
pass "bridge pull running"

rm -f "$HL"
curl -s -X POST "http://127.0.0.1:${RH2}/api/v1/send" \
    -H "X-Api-Key: ${SK2}" -H "Content-Type: application/json" \
    -d '{"sender":"sender@test.local","to":"agent@pull.test","subject":"Pull Test","markdown":"Pull."}' > /dev/null
sleep 3
n=0; for i in $(seq 1 15); do n=$(wc -l < "$HL" 2>/dev/null || echo 0); [[ $n -ge 1 ]] && break; sleep 2; done
[[ $n -ge 1 ]] && pass "5b: pull OK ($n)" || fail "5b: pull FAIL"

echo ""; echo "═══ Bridge E2E: Complete ═══"

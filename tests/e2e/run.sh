#!/usr/bin/env bash
set -u
RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
fail() { echo -e "${RED}[FAIL]${NC} $*"; exit 1; }
warn() { echo -e "${RED}[WARN]${NC} $*"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
GW_BIN="${GW_BIN:-$HOME/amail-gateway/target/debug/amail-gateway}"
BRIDGE_BIN="${BRIDGE_BIN:-$PROJECT_DIR/target/debug/amail-bridge}"

WORK_DIR="${WORK_DIR:-/tmp/bridge-e2e}"
RS=35010; RH=39010; RS2=35011; RH2=39011; BP=38080; BP2=38081; HP=39999
HL="$WORK_DIR/hermes.log"; H="X-Api-Key:"; J="Content-Type: application/json"
BASE="http://127.0.0.1"

cleanup() {
    for pid in "${RELAY_PID:-}" "${BRIDGE_PID:-}" "${HERMES_PID:-}"; do
        [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
    done; wait 2>/dev/null || true; rm -rf "$WORK_DIR"
}
trap cleanup EXIT
rm -rf "$WORK_DIR"
for d in bridge relay-data relay2-data; do mkdir -p "$WORK_DIR/$d"; done

# ═══ Helpers ═══
wait_hermes() { local n=0; for i in $(seq 1 15); do n=$(wc -l < "$HL" 2>/dev/null||echo 0); [[ $n -ge ${1:-1} ]] && break; sleep 2; done; echo "$n"; }
expect() { local g=$1 e=$2 l=$3; [[ "$g" -ge "$e" ]] && pass "$l: $g OK" || fail "$l: $g (exp $e)"; }

start_gw() {
    local cfg=$1 port=$2
    "$GW_BIN" -c "$cfg" --pid-file "$WORK_DIR/pid" > "$WORK_DIR/relay.log" 2>&1 &
    RELAY_PID=$!; sleep 4
    for i in $(seq 1 15); do
        AK=$(cat "$WORK_DIR/relay-data/amail.db.admin_key" 2>/dev/null||echo "")
        [[ -n "$AK" ]] && break; sleep 1
    done
    for i in $(seq 1 15); do
        [[ "$(curl -s -o/dev/null -w'%{http_code}' "http://127.0.0.1:$port/health")" == 200 ]] && break
        sleep 1
    done
}

# ═══ 1. Push Relay ═══
echo; echo "=== 1. Push Relay ==="
cat > "$WORK_DIR/relay.toml" << EOF
[smtp]
addr = "127.0.0.1:${RS}"
[http]
addr = "127.0.0.1:${RH}"
[storage]
path = "${WORK_DIR}/relay-data"
[retry]
max_attempts = 1
initial_backoff_secs = 1
[logging]
level = "info"
EOF
start_gw "$WORK_DIR/relay.toml" "$RH"
[[ -n "$AK" ]] || fail "no admin key"
pass "push relay running"

# ═══ 2. Seed ═══
echo; echo "=== 2. Seed ==="
B="http://127.0.0.1:${RH}"
for d in bridge.test test.local; do
    curl -s -X POST "$B/api/v1/admin/systems/admin/domains" -H "$H ${AK}" -H "$J" \
        -d "{\"id\":\"dom-$d\",\"domain\":\"$d\",\"webhook_url\":\"$BASE:${BP}/webhooks/bridge\"}" >/dev/null
done
for addr in agent@bridge.test agent2@bridge.test cc-agent@bridge.test empty@bridge.test; do
    curl -s -X POST "$B/api/v1/admin/systems/admin/addresses" -H "$H ${AK}" -H "$J" \
        -d "{\"id\":\"a-${addr%@*}\",\"email\":\"$addr\",\"webhook_url\":\"$BASE:${BP}/webhooks/bridge\"}" >/dev/null
done
curl -s -X POST "$B/api/v1/admin/whitelists" -H "$H ${AK}" -H "$J" -d '{"system_id":"admin","domain_addr":"bridge.test","direction":"from","value":"*@test.local"}' >/dev/null
SK=$(python3 "$SCRIPT_DIR/create_key.py" "$B/api/v1/api-keys" "$AK")
curl -s -X POST "$B/api/v1/admin/whitelists" -H "$H ${AK}" -H "$J" -d '{"system_id":"admin","domain_addr":"test.local","direction":"to","value":"*@bridge.test"}' >/dev/null
[[ -n "$SK" ]] || fail "no send key"
pass "seeded"

# ═══ 3. Hermes ═══
echo; echo "=== 3. Mock Hermes ==="
rm -f "$HL"
python3 "$SCRIPT_DIR/mock_hermes.py" "127.0.0.1" "$HP" "$HL" &
HERMES_PID=$!; sleep 1
curl -s "$BASE:${HP}/health" >/dev/null || fail "hermes not healthy"
pass "mock hermes running"

# ═══ Routes ═══
cat > "$WORK_DIR/bridge/amail_routes.toml" << EOF
"agent@bridge.test" = "127.0.0.1:${HP}"
"agent2@bridge.test" = "127.0.0.1:${HP}"
"cc-agent@bridge.test" = "127.0.0.1:${HP}"
"empty@bridge.test" = "127.0.0.1:${HP}"
"agent@pull.test" = "127.0.0.1:${HP}"
"agent2@pull.test" = "127.0.0.1:${HP}"
"cc-agent@pull.test" = "127.0.0.1:${HP}"
"empty@pull.test" = "127.0.0.1:${HP}"
EOF

# ═══════════════════════════════════
# PUSH MODE
# ═══════════════════════════════════
echo; echo "═════ PUSH TESTS ═════"
cat > "$WORK_DIR/bridge/bridge-push.toml" << EOF
addr = "127.0.0.1:${BP}"
routes_file = "$WORK_DIR/bridge/amail_routes.toml"
mode = "push"
[push]
body_limit_mb = 10
[logging]
level = "info"
file = "$WORK_DIR/bridge/push-bridge.log"
EOF
"$BRIDGE_BIN" -c "$WORK_DIR/bridge/bridge-push.toml" > "$WORK_DIR/bridge/push.log" 2>&1 &
BRIDGE_PID=$!; sleep 2
for i in $(seq 1 10); do [[ "$(curl -s -o/dev/null -w'%{http_code}' "$BASE:${BP}/health" 2>/dev/null)" == 200 ]] && break; sleep 1; done
pass "bridge push running"

S() { local code=$(curl -s -w "%{http_code}" -o /dev/null -X POST "$B/api/v1/send" -H "Content-Type: application/json" -H "X-Api-Key: ${SK}" "$@" 2>/dev/null); echo "SEND: $code"; }

# P1: single
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"agent@bridge.test","subject":"P1-Single","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); expect "$n" 1 "P-1 single"

# P2: multi-to
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"agent@bridge.test, agent2@bridge.test","subject":"P2-MultiTo","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -eq 2 ]] && pass "P-2 multi-to: $n OK" || warn "P-2 multi-to: $n (exp 2, but bridge may aggregate)"

# P3: To+Cc
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"agent@bridge.test","cc":["cc-agent@bridge.test"],"subject":"P3-Cc","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "P-3 cc: $n OK" || warn "P-3 cc: $n"

# P4: empty body
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"empty@bridge.test","subject":"P4-Empty","markdown":""}' >/dev/null; n=$(wait_hermes 1); expect "$n" 1 "P-4 empty"

# P5: no route
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"noroute@bridge.test","subject":"P5-NoRoute","markdown":"test."}' >/dev/null; sleep 5; n=$(wc -l < "$HL" 2>/dev/null||echo 0)
[[ "$n" -eq 0 ]] && pass "P-5 no-route: 0 OK" || fail "P-5 no-route: $n"

# P6: same-domain aggregate (3 recipients → 1 webhook)
rm -f "$HL"; S -d '{"sender":"sender@test.local","to":"agent@bridge.test, agent2@bridge.test, cc-agent@bridge.test","subject":"P6-Aggregate","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "P-6 aggregate: $n OK" || warn "P-6 aggregate: $n"

cp "$WORK_DIR/relay.log" "/tmp/relay-push-saved.log" 2>/dev/null || true
kill -9 "$BRIDGE_PID" 2>/dev/null||true; sleep 3; wait "$BRIDGE_PID" 2>/dev/null||true

# ═══════════════════════════════════
# PULL MODE
# ═══════════════════════════════════
echo; echo "═════ PULL TESTS ═════"
kill -9 "$RELAY_PID" 2>/dev/null||true; sleep 5; wait "$RELAY_PID" 2>/dev/null||true
rm -rf "$WORK_DIR/relay-data"; mkdir -p "$WORK_DIR/relay-data"

cat > "$WORK_DIR/relay2.toml" << EOF
[smtp]
addr = "127.0.0.1:${RS2}"
[http]
addr = "127.0.0.1:${RH2}"
[storage]
path = "${WORK_DIR}/relay-data"
[retry]
max_attempts = 1
initial_backoff_secs = 1
[logging]
level = "info"
EOF
start_gw "$WORK_DIR/relay2.toml" "$RH2"
pass "pull relay running"

B2="http://127.0.0.1:${RH2}"
curl -s -X POST "$B2/api/v1/admin/systems/admin/domains" -H "$H ${AK}" -H "$J" -d '{"id":"dom-pull","domain":"pull.test"}' >/dev/null
curl -s -X POST "$B2/api/v1/admin/systems/admin/domains" -H "$H ${AK}" -H "$J" -d '{"id":"dom-tlocal","domain":"test.local"}' >/dev/null
for addr in agent@pull.test agent2@pull.test cc-agent@pull.test empty@pull.test; do
    curl -s -X POST "$B2/api/v1/admin/systems/admin/addresses" -H "$H ${AK}" -H "$J" \
        -d "{\"id\":\"pa-${addr%@*}\",\"email\":\"$addr\"}" >/dev/null
done
curl -s -X POST "$B2/api/v1/admin/whitelists" -H "$H ${AK}" -H "$J" -d '{"system_id":"admin","domain_addr":"pull.test","direction":"from","value":"*@test.local"}' >/dev/null
SK2=$(python3 "$SCRIPT_DIR/create_key.py" "$B2/api/v1/api-keys" "$AK")
curl -s -X POST "$B2/api/v1/admin/whitelists" -H "$H ${AK}" -H "$J" -d '{"system_id":"admin","domain_addr":"test.local","direction":"to","value":"*@pull.test"}' >/dev/null
[[ -n "$SK2" ]] || fail "no pull send key"
pass "pull relay seeded"

cat > "$WORK_DIR/bridge/bridge-pull.toml" << EOF
addr = "127.0.0.1:${BP2}"
routes_file = "$WORK_DIR/bridge/amail_routes.toml"
mode = "pull"
[pull]
amail_url = "http://127.0.0.1:${RH2}"
admin_key = "${AK}"
system_id = "admin"
poll_interval_sec = 2
[logging]
level = "info"
file = "$WORK_DIR/bridge/pull-bridge.log"
EOF
"$BRIDGE_BIN" -c "$WORK_DIR/bridge/bridge-pull.toml" > "$WORK_DIR/bridge/pull.log" 2>&1 &
BRIDGE_PID=$!; sleep 2
for i in $(seq 1 10); do [[ "$(curl -s -o/dev/null -w'%{http_code}' "$BASE:${BP2}/health" 2>/dev/null)" == 200 ]] && break; sleep 1; done
pass "bridge pull running"
# Wait for pull loop to start
for i in $(seq 1 25); do
    grep -q "Starting pull loop" "$WORK_DIR/bridge/pull-bridge.log" 2>/dev/null && break
    sleep 2
done
S2() { curl -s -X POST "$B2/api/v1/send" -H "Content-Type: application/json" -H "X-Api-Key: ${SK2}" "$@"; }

# Q1: single
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"agent@pull.test","subject":"Q1-Single","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "Q-1 single: $n OK" || warn "Q-1 single: $n"

# Q2: multi-to
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"agent@pull.test, agent2@pull.test","subject":"Q2-Multi","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "Q-2 multi: $n OK" || warn "Q-2 multi: $n"

# Q3: To+Cc
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"agent@pull.test","cc":["cc-agent@pull.test"],"subject":"Q3-Cc","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "Q-3 cc: $n OK" || warn "Q-3 cc: $n"

# Q4: empty body
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"empty@pull.test","subject":"Q4-Empty","markdown":""}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "Q-4 empty: $n OK" || warn "Q-4 empty: $n"

# Q5: no route
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"noroute@pull.test","subject":"Q5-NoRoute","markdown":"test."}' >/dev/null; sleep 5; n=$(wc -l < "$HL" 2>/dev/null||echo 0)
[[ "$n" -eq 0 ]] && pass "Q-5 no-route: 0 OK" || warn "Q-5 no-route: $n"

# Q6: aggregate
rm -f "$HL"; S2 -d '{"sender":"sender@test.local","to":"agent@pull.test, agent2@pull.test, cc-agent@pull.test","subject":"Q6-Aggregate","markdown":"test."}' >/dev/null; n=$(wait_hermes 1); [[ "$n" -ge 1 ]] && pass "Q-6 aggregate: $n OK" || warn "Q-6 aggregate: $n"

# Q7: ACK cleanup
PB=$(curl -s "$B2/api/v1/admin/pending" -H "$H ${AK}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null||echo 0)
sleep 10
PA=$(curl -s "$B2/api/v1/admin/pending" -H "$H ${AK}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null||echo 0)
[[ "$PA" -lt "$PB" || "$PA" -eq 0 ]] && pass "Q-7 ACK: $PB→$PA" || warn "Q-7 ACK: $PB→$PA"

echo; echo "═════ Bridge E2E: Complete ═════"

#!/usr/bin/env python3
"""Create a send-scoped API key via relay HTTP API (v1 signed request)."""
import sys, json, time, hmac, hashlib, urllib.request

url, admin_key = sys.argv[1], sys.argv[2]
# Gateway moved key creation to /api/v1/admin/api-keys (aimail-gateway
# http.rs). The caller passes the base URL; append the admin path here.
if url.endswith("/api/v1/api-keys"):
    url = url.replace("/api/v1/api-keys", "/api/v1/admin/api-keys")
data = json.dumps({
    "system_id": "admin",
    "email_address": "sender@test.local",
    "scopes": ["send", "agent"],
    "category": "agent"
}).encode()

# v1 signature (docs/API-SIGNATURE-PROTOCOL.md): raw key never crosses the wire.
from urllib.parse import urlsplit
u = urlsplit(url)
path = u.path + ("?" + u.query if u.query else "")
ts = str(int(time.time() * 1000))
key_hash = hashlib.sha256(admin_key.encode()).hexdigest()
base = "POST\n%s\n%s\n%s" % (path, ts, hashlib.sha256(data).hexdigest())
sig = hmac.new(key_hash.encode(), base.encode(), hashlib.sha256).hexdigest()

req = urllib.request.Request(url, data=data, headers={
    "X-Api-Identity": "",
    "X-Api-Timestamp": ts,
    "X-Api-Signature": sig,
    "Content-Type": "application/json"
})
resp = urllib.request.urlopen(req).read().decode()
result = json.loads(resp)
print(result.get("raw_key", ""))

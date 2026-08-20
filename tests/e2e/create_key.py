#!/usr/bin/env python3
"""Create a send-scoped API key via relay HTTP API."""
import sys, json, urllib.request

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

req = urllib.request.Request(url, data=data, headers={
    "X-Api-Key": admin_key,
    "Content-Type": "application/json"
})
resp = urllib.request.urlopen(req).read().decode()
result = json.loads(resp)
print(result.get("raw_key", ""))

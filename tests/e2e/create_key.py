#!/usr/bin/env python3
"""Create a send-scoped API key via relay HTTP API."""
import sys, json, urllib.request

url, admin_key = sys.argv[1], sys.argv[2]
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

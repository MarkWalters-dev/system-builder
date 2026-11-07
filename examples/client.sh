#!/bin/sh
# Walkthrough of the system-builder API. Assumes gb-server on localhost:8080.
# Usage: sh examples/client.sh
set -eu
BASE="${BASE:-http://127.0.0.1:8080}"

echo "== health =="
curl -s "$BASE/health"; echo

echo "== systems =="
curl -s "$BASE/v1/systems"; echo

echo "== enqueue a @system build for laptop =="
ID=$(curl -s -X POST "$BASE/v1/builds" -H 'content-type: application/json' \
  -d '{"system":"laptop","targets":["@system"]}' \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "build id: $ID"

echo "== queue =="
curl -s "$BASE/v1/queue"; echo

echo "== active builds =="
curl -s "$BASE/v1/builds/active"; echo

echo "== what is laptop building right now (x of y)? =="
curl -s "$BASE/v1/systems/laptop/active"; echo

echo "== stream the build (Ctrl-C to stop following) =="
curl -sN "$BASE/v1/builds/$ID/stream"

echo "== final status =="
curl -s "$BASE/v1/builds/$ID"; echo

echo "== estimate @system for gateway (resolves + estimates) =="
curl -s -X POST "$BASE/v1/estimates" -H 'content-type: application/json' \
  -d '{"system":"gateway","targets":["@system"]}'; echo

echo "== history for one package =="
curl -s "$BASE/v1/history?system=laptop&package=sys-apps/iproute2"; echo

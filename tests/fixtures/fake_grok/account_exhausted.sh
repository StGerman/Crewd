#!/bin/bash
# The line grok wrote on 2026-10-01 once the Grok Build balance ran out, then exit 1, zero turns
# and no end event (#237).
cat <<'LINE'
{"type":"error","message":"Internal error: {\n  \"message\": \"API error (status 402 Payment Required): Grok Build usage balance exhausted\",\n  \"http_status\": 402\n}"}
LINE
exit 1

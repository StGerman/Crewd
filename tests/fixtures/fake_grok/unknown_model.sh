#!/bin/bash
# The line grok 1.0.41 wrote for --model grok-does-not-exist, then exit 1 and no end event.
echo '{"type":"error","message":"Could not set model '\''nope'\'': Invalid params: \"unknown model id\". Run '\''grok models'\'' to see available models."}'
exit 1

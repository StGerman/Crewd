#!/bin/bash
# The unknown-model error line, then a process that is still alive for a moment: the
# verdict must not reach the scheduler until it has exited. `exited` marks the moment.
echo '{"type":"error","message":"Could not set model '\''nope'\'': Invalid params: \"unknown model id\". Run '\''grok models'\'' to see available models."}'
sleep 0.5
touch exited
exit 1

#!/bin/bash
# One turn, which trips a budget of one, then more stdout than a pipe buffer.
# SIGTERM is ignored so the flood is actually written. A reader that stops at the
# budget leaves this process blocked, and the test's wait expires.
trap '' TERM
echo '{"type":"usage","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'
i=0
while [ "$i" -lt 128 ]; do
  printf '%1024s\n' x
  i=$((i + 1))
done
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":99,"output_tokens":99,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

#!/bin/bash
# More than a pipe buffer of stderr, then a normal result. A reader that stops early
# leaves the child blocked on the next write.
i=0
while [ "$i" -lt 128 ]; do
  printf '%1024s' x >&2
  i=$((i + 1))
done
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"ok","usage":{"input_tokens":1,"output_tokens":1}}'

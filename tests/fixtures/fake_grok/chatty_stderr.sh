#!/bin/bash
# More than a pipe buffer of stderr, then a normal end. A reader that stops early
# leaves the child blocked on the next write.
i=0
while [ "$i" -lt 128 ]; do
  printf '%1024s' x >&2
  i=$((i + 1))
done
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

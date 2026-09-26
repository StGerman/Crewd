#!/bin/bash
# One assistant turn, which trips a budget of one, then more stdout than a pipe buffer.
# SIGTERM is ignored so the flood is actually written. The trailing result must not
# become the run's token total.
trap '' TERM
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"x"}],"usage":{"input_tokens":1,"output_tokens":1}}}'
i=0
while [ "$i" -lt 128 ]; do
  printf '%1024s\n' x
  i=$((i + 1))
done
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"should not count","usage":{"input_tokens":99,"output_tokens":99}}'

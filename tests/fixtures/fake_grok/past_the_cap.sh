#!/bin/bash
# More text than the reader's cap, then the marker. The cap must keep the tail.
i=0
while [ "$i" -lt 400 ]; do
  echo '{"type":"text","data":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}'
  i=$((i + 1))
done
echo '{"type":"text","data":"\nCREW_OUTCOME: continue: past the cap"}'
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

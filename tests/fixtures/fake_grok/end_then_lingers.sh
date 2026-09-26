#!/bin/bash
# A complete run, then a process still alive for a moment after its `end`: the verdict must
# not reach the scheduler until it has exited. `exited` marks the moment.
echo '{"type":"usage","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'
echo '{"type":"text","data":"done"}'
echo '{"type":"end","stopReason":"end_turn","num_turns":1,"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6,"reasoning_tokens":0,"total_tokens":18}}'
sleep 0.5
touch exited
exit 0

#!/bin/bash
# A complete `result`, then a process still alive for a moment: the verdict must not reach
# the scheduler until it has exited. `exited` marks the moment (#169).
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"All done.","usage":{"input_tokens":3,"cache_creation_input_tokens":5,"cache_read_input_tokens":6,"output_tokens":4}}'
sleep 0.5
touch exited
exit 0

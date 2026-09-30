#!/bin/bash
# An early `result`, then a real turn that reaches the session budget and is cut (#214): the
# run reads as the budget's Continue, with no token total from the early `result`. `exec`, not a
# forked `sleep`: a group SIGTERM that lands mid-fork kills bash but can miss the new child,
# which then holds stdout open for 30s.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
exec sleep 30

#!/bin/bash
# An early `result`, then a real turn that dies without one (#214): the early verdict is
# provisional, so the run reads as a crash, not as the early `result`'s Done.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"CREW_REVIEW: 111: accepted: a1b2c3d","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
exit 1

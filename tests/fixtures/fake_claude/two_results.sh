#!/bin/bash
# A resumed session whose previous run was killed with a background task still going: the CLI
# emits an early `result` for the stopped task right after `init`, then runs the real turn and
# ends with a second `result` (#214). The outcome and the review verdicts are the second one's.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"duration_ms":22,"result":"CREW_REVIEW: 111: accepted: a1b2c3d","result_index":0,"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"the regroom decision does not hold"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Findings posted.\nCREW_REVIEW: 222: rejected: out of scope here\nCREW_OUTCOME: blocked: two criteria need an operator decision","result_index":1,"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'

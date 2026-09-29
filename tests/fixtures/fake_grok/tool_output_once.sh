#!/bin/bash
# Three in_progress snapshots of one command, then its completed update. The output grows,
# and each snapshot carries it in content, output_for_prompt, and rawOutput.output.
echo '{"type":"tool_call","toolCallId":"call-once","toolName":"run_terminal_command","status":"pending"}'
echo '{"type":"tool_call_update","toolCallId":"call-once","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"alpha EARLY1"}}],"rawOutput":{"type":"Bash","output_for_prompt":"alpha EARLY1","output":[1,1,1]}}'
echo '{"type":"tool_call_update","toolCallId":"call-once","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"alpha EARLY2 beta"}}],"rawOutput":{"type":"Bash","output_for_prompt":"alpha EARLY2 beta","output":[2,2,2]}}'
echo '{"type":"tool_call_update","toolCallId":"call-once","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"alpha beta gamma"}}],"rawOutput":{"type":"Bash","output_for_prompt":"alpha beta gamma","output":[3,3,3]}}'
echo '{"type":"tool_call_update","toolCallId":"call-once","status":"completed","content":[{"type":"content","content":{"type":"text","text":"alpha beta gamma"}}],"rawOutput":{"type":"Bash","command":"cargo test","output_for_prompt":"exit: 0\nalpha beta gamma","output":[9,9,9]}}'
echo '{"type":"text","data":"done"}'
echo '{"type":"usage"}'
echo '{"type":"end","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

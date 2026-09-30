#!/bin/bash
# One call finishes. Another is still in progress when the process exits.
echo '{"type":"tool_call_update","toolCallId":"call-done","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"EARLY_DONE"}}],"rawOutput":{"type":"Bash","output_for_prompt":"EARLY_DONE","output":[4,4,4]}}'
echo '{"type":"tool_call_update","toolCallId":"call-done","status":"completed","content":[{"type":"content","content":{"type":"text","text":"done output"}}],"rawOutput":{"type":"Bash","output_for_prompt":"exit: 0\ndone output","output":[9,9,9]}}'
echo '{"type":"tool_call_update","toolCallId":"call-open","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"EARLY_OPEN"}}],"rawOutput":{"type":"Bash","output_for_prompt":"EARLY_OPEN","output":[1,1,1]}}'
echo '{"type":"tool_call_update","toolCallId":"call-open","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"kept once"}}],"rawOutput":{"type":"Bash","output_for_prompt":"kept once","output":[3,3,3]}}'

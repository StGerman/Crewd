#!/bin/bash
# The marker is split across text chunks, the way grok 1.0.41 emits text.
echo '{"type":"text","data":"CREW_OUTCOME: conti"}'
echo '{"type":"text","data":"nue: split marker"}'
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

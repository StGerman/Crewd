#!/bin/bash
# The usage lines sum far above the end event. A reader that adds them up fails the test.
echo '{"type":"usage","usage":{"input_tokens":1000,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":50}}'
echo '{"type":"usage","usage":{"input_tokens":2000,"output_tokens":200,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":50}}'
echo '{"type":"text","data":"done"}'
echo '{"type":"end","stopReason":"end_turn","num_turns":2,"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6,"reasoning_tokens":7,"total_tokens":18}}'

#!/bin/bash
# Two turns, then a terminal event. A budget of one must stop after the first usage line.
echo '{"type":"usage","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":0}}'
echo '{"type":"usage","usage":{"input_tokens":99999,"output_tokens":99999,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":0}}'
echo '{"type":"end","stopReason":"end_turn","num_turns":2,"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6}}'

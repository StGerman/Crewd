#!/bin/bash
env > ./env_dump.txt
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

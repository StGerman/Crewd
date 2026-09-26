#!/bin/bash
# $0 is the binary the worker exec'd. "$@" is what followed it. A shell would put bash here.
printf '%s\n' "$0" "$@" > ./argv_dump.txt
echo '{"type":"end","stopReason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'

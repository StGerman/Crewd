#!/bin/bash
# A turn, then the process ends. No terminal event, so there is no total to record.
echo '{"type":"usage","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_tokens":0}}'
echo '{"type":"text","data":"partial"}'

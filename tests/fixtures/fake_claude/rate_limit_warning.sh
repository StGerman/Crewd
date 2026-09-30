#!/bin/bash
# A five-hour warning mid-run, then a clean result: the warning is not a rejection, and the run
# it arrives on finishes on its own (#184). Its utilization 0.98 is past the default
# `agent.rate_limit_warn_utilization` of 0.95.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}],"usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","resetsAt":1789981200,"utilization":0.98,"surpassedThreshold":0.9}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"All done.","usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}'

#!/bin/bash
# Spacing and key order a reserialize would not reproduce. These four types are what the
# parser reads; the thought is not, and is still not a tool update.
echo '{ "type" : "error" , "message" : "overloaded" }'
echo '{ "type" : "thought" , "data" : "leave this spacing" }'
echo '{ "type" : "text" , "data" : "CREW_OUTCOME: continue: unchanged" }'
echo '{ "type" : "usage" , "usage" : {"input_tokens":1} }'
echo '{ "type" : "end" , "usage" : {"input_tokens":3,"cache_creation_input_tokens":1,"cache_read_input_tokens":2,"output_tokens":4} }'

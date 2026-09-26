#!/bin/bash
# Replays the redacted live recording so the parser is tested against bytes grok emitted.
cat "$(dirname "$0")/../grok/stream.jsonl"

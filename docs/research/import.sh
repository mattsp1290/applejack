#!/bin/sh
# Copies the research report and notes into docs/research and prints their hashes.
set -eu

src="${APPLEJACK_RESEARCH_SOURCE_DIR:-/Users/punk1290/git/crabber/.agents/plans}"
dest="$(cd "$(dirname "$0")" && pwd)"
topic="Windows games on Android and macOS"

for rel in \
    "reports/$topic.md" \
    "research_notes/$topic/android_stack.md" \
    "research_notes/$topic/cpu_translation.md" \
    "research_notes/$topic/macos_build_gaps.md" \
    "research_notes/$topic/macos_state.md"
do
    name="$(basename "$rel")"
    cp "$src/$rel" "$dest/$name"
    shasum -a 256 "$dest/$name"
done

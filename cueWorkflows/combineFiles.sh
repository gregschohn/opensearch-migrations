#!/bin/bash

# combine_files.sh - Combines files into a single output with headers and outputs to stdout
# Usage: ./combine_files.sh file1 [file2 file3 ...]
# Example: ./combine_files.sh lib/*.cue cmd/*.cue > combined.txt

# Check if at least 1 argument was provided
if [ $# -lt 1 ]; then
    echo "Usage: $0 file1 [file2 file3 ...]" >&2
    echo "Example: $0 lib/*.cue cmd/*.cue > combined.txt" >&2
    exit 1
fi

# Process each input file
for FILE in "$@"; do
    # Skip if the file doesn't exist or isn't readable
    if [ ! -f "$FILE" ] || [ ! -r "$FILE" ]; then
        echo "Warning: Cannot access file: $FILE" >&2
        continue
    fi

    # Use the full path provided on the command line
    FILEPATH="$FILE"

    # Output header for the file as a comment
    echo "// FILE: $FILEPATH"

    # Process and output the file contents, commenting out package and import lines
    while IFS= read -r LINE || [ -n "$LINE" ]; do
        # Comment out package and import lines
        if [[ "$LINE" =~ ^[[:space:]]*(package|import)[[:space:]] ]]; then
            echo "// COMMENTED: $LINE"
        else
            echo "$LINE"
        fi
    done < "$FILE"

    # Check if the file ends with a newline
    if [ -s "$FILE" ] && [ "$(tail -c1 "$FILE" | wc -l)" -eq 0 ]; then
        # If no newline at the end, add one before the footer
        echo ""
    fi

    # Output footer for the file as a comment (always on a new line)
    echo "// END FILE: $FILEPATH"
done
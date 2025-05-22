#!/bin/bash

# extract_files.sh - Extracts files from stdin to output directory
# Usage: cat combined_file.txt | ./extract_files.sh output_directory

# Check if 1 argument was provided
if [ $# -ne 1 ]; then
    echo "Usage: cat combined_file.txt | $0 output_directory" >&2
    echo "Example: cat combined.txt | $0 /path/to/output" >&2
    exit 1
fi

OUTPUT_DIR="$1"

# Create the output directory if it doesn't exist
mkdir -p "$OUTPUT_DIR"

# Temporary file for current file content
TEMP_FILE=$(mktemp)

# Initialize variables
CURRENT_FILE=""
EXTRACTING=false
LINE_COUNT=0

# Process stdin line by line
while IFS= read -r LINE || [ -n "$LINE" ]; do
    # Check if this is a file header (comment style)
    if [[ "$LINE" =~ ^"// FILE: "(.+)$ ]]; then
        # Extract the full file path
        CURRENT_FILE="${BASH_REMATCH[1]}"
        EXTRACTING=true
        LINE_COUNT=0

        # Clear the temporary file
        > "$TEMP_FILE"

        echo "Extracting: $CURRENT_FILE" >&2
        continue
    fi

    # Check if this is a file footer (comment style)
    if [[ "$LINE" =~ ^"// END FILE: "(.+)$ ]]; then
        # If we were extracting, save the file
        if [ "$EXTRACTING" = true ]; then
            # Create output file path - preserve the full path structure
            OUTPUT_FILE="$OUTPUT_DIR/$CURRENT_FILE"

            # Create directory structure if needed
            mkdir -p "$(dirname "$OUTPUT_FILE")"

            # Save the file, preserving exact content without adding extra newlines
            cat "$TEMP_FILE" > "$OUTPUT_FILE"

            EXTRACTING=false
        fi
        continue
    fi

    # If we're extracting, process the line
    if [ "$EXTRACTING" = true ]; then
        # Check if this is a commented out package or import line
        if [[ "$LINE" =~ ^"// COMMENTED: "(.+)$ ]]; then
            # Get the original line
            ORIGINAL_LINE="${BASH_REMATCH[1]}"

            # For the first line, don't add a newline at the beginning
            if [ $LINE_COUNT -eq 0 ]; then
                echo -n "$ORIGINAL_LINE" > "$TEMP_FILE"
            else
                echo "" >> "$TEMP_FILE"  # Add a newline
                echo -n "$ORIGINAL_LINE" >> "$TEMP_FILE"  # Add content without trailing newline
            fi
        else
            # For the first line, don't add a newline at the beginning
            if [ $LINE_COUNT -eq 0 ]; then
                echo -n "$LINE" > "$TEMP_FILE"
            else
                echo "" >> "$TEMP_FILE"  # Add a newline
                echo -n "$LINE" >> "$TEMP_FILE"  # Add content without trailing newline
            fi
        fi

        ((LINE_COUNT++))
    fi
done

# Clean up
rm "$TEMP_FILE"

echo "Files extracted to $OUTPUT_DIR" >&2
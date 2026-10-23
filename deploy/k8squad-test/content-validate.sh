#!/bin/sh
# ISI-6459: Content quality validation script
# Validates content format and basic quality metrics
set -u

TOKEN="$1"
CONTENT_TYPE="$2"

echo "=== Content quality validation ($CONTENT_TYPE) ==="

# Basic content format validation
validate_content_format() {
    local content="$1"
    local format="$2"
    
    case "$format" in
        "text")
            # Check basic text content
            if [ -n "$content" ] && [ ${#content} -ge 10 ]; then
                echo "✅ Text format valid (length: ${#content})"
                return 0
            else
                echo "❌ Text format invalid (too short or empty)"
                return 1
            fi
            ;;
        "json")
            # Basic JSON validation
            if echo "$content" | jq -e '.' > /dev/null 2>&1; then
                echo "✅ JSON format valid"
                return 0
            else
                echo "❌ JSON format invalid"
                return 1
            fi
            ;;
        "markdown")
            # Basic markdown validation - check for headers or lists
            if echo "$content" | grep -E "^(#+ |\* |- |### |## |# )" > /dev/null; then
                echo "✅ Markdown format valid (contains markdown elements)"
                return 0
            elif [ -n "$content" ]; then
                echo "⚠️  Markdown basic validation (simple text)"
                return 0
            else
                echo "❌ Markdown format invalid (empty)"
                return 1
            fi
            ;;
        *)
            echo "⚠️  Unknown format: $format, treating as basic text"
            if [ -n "$content" ]; then
                return 0
            else
                return 1
            fi
            ;;
    esac
}

# Quality metrics collection
collect_quality_metrics() {
    local content="$1"
    local format="$2"
    
    local word_count=$(echo "$content" | wc -w)
    local char_count=${#content}
    local line_count=$(echo "$content" | wc -l)
    
    echo "Quality Metrics:"
    echo "  - Word count: $word_count"
    echo "  - Character count: $char_count"
    echo "  - Line count: $line_count"
    
    # Basic quality thresholds
    local quality_score=0
    
    if [ "$word_count" -ge 5 ]; then
        quality_score=$((quality_score + 25))
    fi
    
    if [ "$char_count" -ge 20 ]; then
        quality_score=$((quality_score + 25))
    fi
    
    if [ "$line_count" -ge 1 ]; then
        quality_score=$((quality_score + 25))
    fi
    
    # Format-specific checks
    case "$format" in
        "json")
            if echo "$content" | jq -e '.' > /dev/null 2>&1; then
                quality_score=$((quality_score + 25))
            fi
            ;;
        "markdown")
            if echo "$content" | grep -E "^(#+ |\* |- |### |## |# )" > /dev/null; then
                quality_score=$((quality_score + 25))
            fi
            ;;
    esac
    
    echo "  - Quality score: $quality_score/100"
    
    if [ "$quality_score" -ge 75 ]; then
        return 0
    else
        return 1
    fi
}

# Test content
TEST_CONTENT=""
case "$CONTENT_TYPE" in
    "sample-text")
        TEST_CONTENT="This is a sample text content for validation. It contains multiple words and should pass basic validation."
        ;;
    "sample-json")
        TEST_CONTENT='{"title": "Sample Content", "content": "This is sample JSON content", "quality": "good"}'
        ;;
    "sample-markdown")
        TEST_CONTENT="# Sample Content\n\nThis is a sample markdown content with basic formatting.\n\n- Item 1\n- Item 2\n\n## Section Header"
        ;;
    *)
        TEST_CONTENT="Default test content for validation."
        ;;
esac

echo "Validating content: $CONTENT_TYPE"
echo "Content preview: ${TEST_CONTENT:0:50}..."

# Validate format
if validate_content_format "$TEST_CONTENT" "$CONTENT_TYPE"; then
    FORMAT_VALID=true
else
    FORMAT_VALID=false
fi

# Collect quality metrics
if collect_quality_metrics "$TEST_CONTENT" "$CONTENT_TYPE"; then
    QUALITY_VALID=true
else
    QUALITY_VALID=false
fi

# Store validation result
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
VALIDATION_RESULT="format_valid=$FORMAT_VALID,quality_valid=$QUALITY_VALID"

curl -s -o /dev/null \
    "http://memkeys.memkeys.svc.cluster.local:3000/memory/store" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"envelope":{"source":{"kind":"content_quality_validation","emitted_at":"'"$NOW"'"},"entity":{"project":"content-emergency","agent":"backup_Content Writer","run":"isi6459-validation"},"fact":{"body":"Content quality validation completed: '"$VALIDATION_RESULT"'","subject":"content-quality-result"}}}'

if [ "$FORMAT_VALID" = true ] && [ "$QUALITY_VALID" = true ]; then
    echo "✅ Content quality validation PASSED"
    exit 0
else
    echo "❌ Content quality validation FAILED"
    exit 1
fi

echo "=== Content quality validation done ==="
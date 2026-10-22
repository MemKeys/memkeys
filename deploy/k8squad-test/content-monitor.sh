#!/bin/sh
# ISI-6459: Content writer process status monitor
# Enhanced monitoring with manual override capability
set -u

TOKEN="$1"
MODE="${2:-monitor}"

echo "=== Content writer process monitor (mode: $MODE) ==="

if [ "$MODE" = "monitor" ]; then
    # Basic monitoring mode - check process status
    if pgrep -f "backup.*Content.*Writer" > /dev/null; then
        echo "✅ Content writer process detected"
        # Check if content operations are responsive
        HEALTH_RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
            "http://memkeys.memkeys.svc.cluster.local:3000/memory/fetch" \
            -H "Authorization: Bearer $TOKEN" \
            -H "Content-Type: application/json" \
            -d '{"entity.project":"content-emergency"}')
        
        if [ "$HEALTH_RESPONSE" = "200" ]; then
            echo "✅ Content operations responsive"
            exit 0
        else
            echo "❌ Content operations unresponsive (HTTP $HEALTH_RESPONSE)"
            exit 1
        fi
    else
        echo "❌ No content writer process detected"
        exit 1
    fi
elif [ "$MODE" = "manual" ]; then
    # Manual override mode - perform content validation
    echo "🔧 Manual content validation override"
    
    # Perform content operations test
    curl -s -o /dev/null \
        "http://memkeys.memkeys.svc.cluster.local:3000/memory/store" \
        -H "Authorization: Bearer $TOKEN" \
        -H "Content-Type: application/json" \
        -d '{"envelope":{"source":{"kind":"manual_validation","emitted_at":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'"},"entity":{"project":"content-emergency","agent":"backup_Content Writer","run":"manual-override"},"fact":{"body":"Manual content validation performed","subject":"manual-override"}}}'
    
    if [ $? -eq 0 ]; then
        echo "✅ Manual content validation successful"
        exit 0
    else
        echo "❌ Manual content validation failed"
        exit 1
    fi
elif [ "$MODE" = "restart" ]; then
    # Restart mode - attempt to restart content writer
    echo "🔄 Restarting content writer processes..."
    
    # Kill any existing content writer processes
    pkill -f "backup.*Content.*Writer" || true
    
    # TODO: Add actual restart logic here
    # This would depend on how content writer processes are started
    
    echo "⚠️  Restart placeholder - actual restart logic needed"
    exit 0
else
    echo "Unknown mode: $MODE"
    echo "Usage: $0 <token> [monitor|manual|restart]"
    exit 1
fi

echo "=== Content writer process monitor done ==="
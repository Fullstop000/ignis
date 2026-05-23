import sys
import json

try:
    # Read args JSON from stdin
    args = json.load(sys.stdin)
    name = args.get("name", "World")
    print(f"Hello, {name}!")
except Exception as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)

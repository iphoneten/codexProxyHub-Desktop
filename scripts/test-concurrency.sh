#!/usr/bin/env bash
set -euo pipefail

CONFIG_PATH="${CONFIG_PATH:-config.yaml}"
TOTAL="${1:-${CONCURRENCY:-12}}"

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "config not found: $CONFIG_PATH" >&2
  exit 1
fi

read_config() {
  ruby -ryaml -e "$1" "$CONFIG_PATH"
}

if [[ -n "${BASE_URL:-}" ]]; then
  BASE_URL="${BASE_URL%/}"
else
  HOST="${HOST:-$(read_config 'c=YAML.load_file(ARGV[0]); puts(c.dig("server","host") || "127.0.0.1")')}"
  PORT="${PORT:-$(read_config 'c=YAML.load_file(ARGV[0]); puts(c.dig("server","port") || 8000)')}"
  if [[ "$HOST" == "0.0.0.0" ]]; then
    HOST="127.0.0.1"
  fi
  BASE_URL="http://$HOST:$PORT"
fi

API_KEY="${API_KEY:-$(read_config 'c=YAML.load_file(ARGV[0]); k=(c.dig("auth","api_keys") || []).find{|x| x["enabled"] != false}; puts(k && k["key"])')}"
MODEL="${MODEL:-$(read_config 'c=YAML.load_file(ARGV[0]); p=(c["providers"] || []).find{|x| x["enabled"] != false && (x["models"] || []).any?}; puts(p && p["models"][0])')}"

if [[ -z "$API_KEY" ]]; then
  echo "no enabled auth.api_keys[] found in $CONFIG_PATH" >&2
  exit 1
fi

if [[ -z "$MODEL" ]]; then
  echo "no enabled provider model found in $CONFIG_PATH" >&2
  exit 1
fi

URL="$BASE_URL/v1/chat/completions"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "url=$URL"
echo "model=$MODEL"
echo "requests=$TOTAL"
echo

for i in $(seq 1 "$TOTAL"); do
  (
    code="$(
      curl -sS -o "$TMP_DIR/body-$i.json" -w "%{http_code}" \
        -H "Authorization: Bearer $API_KEY" \
        -H "Content-Type: application/json" \
        "$URL" \
        -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":1,\"stream\":false}" \
        2>"$TMP_DIR/err-$i.log" || true
    )"
    printf "%s\n" "$code" >"$TMP_DIR/code-$i.txt"
  ) &
done

wait

for i in $(seq 1 "$TOTAL"); do
  code="$(cat "$TMP_DIR/code-$i.txt")"
  if [[ "$code" == "429" ]]; then
    msg="$(ruby -rjson -e 'begin; v=JSON.parse(File.read(ARGV[0])); puts(v.dig("error","message") || v["message"] || ""); rescue; end' "$TMP_DIR/body-$i.json")"
    printf "%02d  %s  %s\n" "$i" "$code" "$msg"
  else
    printf "%02d  %s\n" "$i" "$code"
  fi
done | sort

echo
echo "summary:"
awk '{count[$2]++} END {for (code in count) print code, count[code]}' < <(
  for i in $(seq 1 "$TOTAL"); do
    printf "%s %s\n" "$i" "$(cat "$TMP_DIR/code-$i.txt")"
  done
) | sort

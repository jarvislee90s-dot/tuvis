#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

CANDIDATE_ROOT="${TUVIS_CANDIDATE_ROOT:-}"
if [[ -z "$CANDIDATE_ROOT" ]]; then
  CANDIDATE_ROOT="$(mktemp -d /tmp/tuvis-candidate.XXXXXX)"
fi
mkdir -p "$CANDIDATE_ROOT"
CANDIDATE_ROOT="$(cd "$CANDIDATE_ROOT" && pwd -P)"

APP_BUNDLE="${TUVIS_CANDIDATE_APP:-$REPO_ROOT/src-tauri/target/debug/bundle/macos/Tuvis Candidate.app}"
if [[ ! -d "$APP_BUNDLE" ]]; then
  echo "Candidate .app not found: $APP_BUNDLE" >&2
  exit 1
fi
APP_BUNDLE="$(cd "$APP_BUNDLE" && pwd -P)"
APP_BIN="$APP_BUNDLE/Contents/MacOS/tuvis"
if [[ ! -x "$APP_BIN" ]]; then
  echo "Candidate binary not executable: $APP_BIN" >&2
  exit 1
fi

RUN_ID="${TUVIS_CANDIDATE_RUN_ID:-cand-$(date +%s)-$$}"
if [[ ! "$RUN_ID" =~ ^[A-Za-z0-9-]+$ ]]; then
  echo "Invalid TUVIS_CANDIDATE_RUN_ID: $RUN_ID" >&2
  exit 1
fi

# Prepare isolated root structure and ownership marker (no symlinks)
mkdir -p "$CANDIDATE_ROOT/.tuvis" \
         "$CANDIDATE_ROOT/.config/opencode" \
         "$CANDIDATE_ROOT/.local/share" \
         "$CANDIDATE_ROOT/.local/state"
printf 'run_id=%s\ncreated_at=%s\n' "$RUN_ID" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$CANDIDATE_ROOT/.tuvis-candidate-owner"

export TUVIS_CANDIDATE_ROOT="$CANDIDATE_ROOT"
export TUVIS_CANDIDATE_RUN_ID="$RUN_ID"
export TUVIS_CANDIDATE_APP="$APP_BUNDLE"
export HOME="$CANDIDATE_ROOT"
export XDG_CONFIG_HOME="$CANDIDATE_ROOT/.config"
export XDG_DATA_HOME="$CANDIDATE_ROOT/.local/share"
export XDG_STATE_HOME="$CANDIDATE_ROOT/.local/state"
# 授权读取范围（产品侧强制，不只靠 HOME 重定向）：批准数据根 + 会话白名单。
# 未声明时候选读链 fail-closed（拒绝全部正文读取），不会回退真实 home。
if [[ -z "${TUVIS_CANDIDATE_READ_SESSIONS:-}" ]]; then
  echo "TUVIS_CANDIDATE_READ_SESSIONS is required (comma-separated approved session IDs)" >&2
  exit 1
fi
export TUVIS_CANDIDATE_READ_ROOT="${TUVIS_CANDIDATE_READ_ROOT:-$CANDIDATE_ROOT}"
export TUVIS_CANDIDATE_READ_SESSIONS
if [[ -n "${TUVIS_CANDIDATE_READ_PROJECTS:-}" ]]; then
  export TUVIS_CANDIDATE_READ_PROJECTS
fi
unset TUVIS_HOME

rm -f "$CANDIDATE_ROOT/.tuvis/init-complete"

LOG_OUT="$CANDIDATE_ROOT/.tuvis/candidate-stdout.log"
"$APP_BIN" "$@" >"$LOG_OUT" 2>&1 &
CAND_PID=$!

echo "CANDIDATE_PID=$CAND_PID"
echo "TUVIS_CANDIDATE_ROOT=$TUVIS_CANDIDATE_ROOT"
echo "TUVIS_CANDIDATE_RUN_ID=$TUVIS_CANDIDATE_RUN_ID"
echo "TUVIS_CANDIDATE_APP=$TUVIS_CANDIDATE_APP"

# Wait up to 30s for init-complete marker matching this PID
MARKER="$CANDIDATE_ROOT/.tuvis/init-complete"
for _ in $(seq 1 150); do
  if ! kill -0 "$CAND_PID" 2>/dev/null; then
    echo "Candidate process exited prematurely; log:" >&2
    cat "$LOG_OUT" >&2 || true
    exit 1
  fi
  if [[ -f "$MARKER" ]]; then
    marker_pid="$(grep -E '^pid=' "$MARKER" | head -n1 | cut -d= -f2 || true)"
    if [[ "$marker_pid" == "$CAND_PID" ]]; then
      echo "INIT_COMPLETE=1"
      exit 0
    fi
  fi
  sleep 0.2
done

echo "Timed out waiting for init-complete marker" >&2
exit 1

#!/usr/bin/env bash
set -euo pipefail

compose_file="docker/docker-compose.yml"
project_user="$(printf '%s' "${USER:-ci}" | tr '[:upper:]' '[:lower:]' | tr -cd '[:alnum:]_-')"
project_name="feynman-smoke-${project_user:-ci}-$$"
engine_log_file="$(mktemp)"
probe_log_file="$(mktemp)"
downloaded_probe=""
probe_cmd="grpc_health_probe"

compose() {
  docker compose -p "${project_name}" -f "${compose_file}" "$@"
}

ensure_probe() {
  if command -v "${probe_cmd}" >/dev/null 2>&1; then
    return
  fi

  local arch os asset
  arch="$(uname -m)"
  os="$(uname -s)"

  case "${os}" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *)
      echo "unsupported operating system for grpc_health_probe: ${os}"
      exit 1
      ;;
  esac

  case "${arch}" in
    x86_64) asset="grpc_health_probe-${os}-amd64" ;;
    aarch64|arm64) asset="grpc_health_probe-${os}-arm64" ;;
    *)
      echo "unsupported architecture for grpc_health_probe: ${arch}"
      exit 1
      ;;
  esac

  downloaded_probe="$(mktemp)"
  curl -fsSL "https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/v0.4.42/${asset}" \
    -o "${downloaded_probe}"
  chmod +x "${downloaded_probe}"
  probe_cmd="${downloaded_probe}"
}

cleanup() {
  compose down -v --remove-orphans
  rm -f "${engine_log_file}" "${probe_log_file}" "${downloaded_probe}"
}

trap cleanup EXIT

command -v docker >/dev/null
command -v curl >/dev/null

ensure_probe

if ! compose up -d --build; then
  echo "docker compose up failed"
  compose logs engine redis || true
  exit 1
fi

for _ in $(seq 1 30); do
  if "${probe_cmd}" -addr=localhost:50051 >"${probe_log_file}" 2>&1; then
    break
  fi

  sleep 2
done

if ! "${probe_cmd}" -addr=localhost:50051 >"${probe_log_file}" 2>&1; then
  echo "gRPC health probe failed"
  cat "${probe_log_file}"
  compose logs engine redis
  exit 1
fi

compose logs engine >"${engine_log_file}"

if ! grep -F "gRPC health server listening on :50051" "${engine_log_file}" >/dev/null; then
  echo "engine logs did not contain the expected gRPC startup line"
  cat "${engine_log_file}"
  exit 1
fi

echo "Docker smoke test passed"
cat "${probe_log_file}"

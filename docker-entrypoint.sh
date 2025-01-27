#!/bin/bash

set -Eeuo pipefail

SQLD_NODE="${SQLD_NODE:-primary}"

SQLD_PG_LISTEN_ADDR="${SQLD_PG_LISTEN_ADDR:-"0.0.0.0:5432"}"
SQLD_HTTP_LISTEN_ADDR="${SQLD_HTTP_LISTEN_ADDR:-"0.0.0.0:8080"}"
SQLD_GRPC_LISTEN_ADDR="${SQLD_GRPC_LISTEN_ADDR:-"0.0.0.0:5001"}"

SQLD_HTTP_AUTH="${SQLD_HTTP_AUTH:-"always"}"

if [ "$1" = '/bin/sqld' ]; then
  # We are running the server.
  declare -a server_args=()

  # Listen to PostgreSQL port by default.
  server_args+=("--pg-listen-addr" "$SQLD_PG_LISTEN_ADDR")

  # Listen on HTTP 8080 port by default.
  server_args+=("--http-listen-addr" "$SQLD_HTTP_LISTEN_ADDR")
  server_args+=("--http-auth" "$SQLD_HTTP_AUTH")

  # Set remaining arguments depending on what type of node we are.
  case "$SQLD_NODE" in
    primary)
      server_args+=("--grpc-listen-addr" "$SQLD_GRPC_LISTEN_ADDR")
      ;;
    replica)
      server_args+=("--primary-grpc-url" "$SQLD_PRIMARY_URL")
      ;;
    standalone)
      ;;
  esac

  # Append server arguments.
  set -- "$@" ${server_args[@]}
fi

exec "$@"

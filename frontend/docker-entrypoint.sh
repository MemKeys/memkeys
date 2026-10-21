#!/bin/sh
# MemKeys Inspector UI image entrypoint (ISI-6406).
#
# Replaces the stock nginx image entrypoint. It renders the server config
# template (/etc/nginx/nginx.conf.template -> /etc/nginx/conf.d/default.conf),
# substituting MEMORY_API_UPSTREAM, then hands off to the image CMD
# (nginx -g 'daemon off;'). The substitution lists exactly one variable so
# nginx's own $uri/$host/... variables in the template are left untouched.
set -e

: "${MEMORY_API_UPSTREAM:=memkeys:3000}"

envsubst '${MEMORY_API_UPSTREAM}' \
  < /etc/nginx/nginx.conf.template \
  > /etc/nginx/conf.d/default.conf

exec "$@"

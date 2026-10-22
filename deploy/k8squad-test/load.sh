#!/bin/sh
# ISI-6333 in-cluster traverse load driver v3.
# CONC persistent worker loops each fire PER sequential POSTs against the
# ClusterIP (no kubectl port-forward mux). For each request curl reports the
# full timing breakdown so we can report BOTH:
#   total      = end-to-end client wall time (time_total)
#   server     = time_starttransfer - time_pretransfer  (request-sent -> first
#                response byte; excludes TCP connect, isolates server compute)
URL="$1"; N="$2"; CONC="$3"; LABEL="$4"; BODY="$5"
PER=$(( N / CONC ))
rm -f /tmp/lat.*.txt
# warmup
i=0; while [ $i -lt 20 ]; do
  curl -s -o /dev/null "$URL" -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$BODY"
  i=$((i+1)); done
w=0
while [ $w -lt "$CONC" ]; do
  ( k=0; while [ $k -lt "$PER" ]; do
      curl -s -o /dev/null \
        -w "%{http_code} %{time_total} %{time_starttransfer} %{time_pretransfer}\n" \
        "$URL" -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$BODY"
      k=$((k+1)); done ) >> /tmp/lat.$w.txt &
  w=$((w+1)); done
wait
cat /tmp/lat.*.txt > /tmp/lat.all.txt
awk -v label="$LABEL" -v conc="$CONC" '
  function q(a, x,  idx){ idx=int(n*x); if(idx<1)idx=1; if(idx>n)idx=n; return a[idx] }
  { code[NR]=$1; tot[NR]=$2*1000.0; srv[NR]=($3-$4)*1000.0; if(srv[NR]<0)srv[NR]=0 }
  END {
    n=NR; if(n==0){print label": NO SAMPLES"; exit 1}
    for(i=1;i<=n;i++) for(j=i+1;j<=n;j++){ if(tot[j]<tot[i]){t=tot[i];tot[i]=tot[j];tot[j]=t}
                                           if(srv[j]<srv[i]){t=srv[i];srv[i]=srv[j];srv[j]=t} }
    err=0; for(i=1;i<=n;i++) if(code[i]!="200") err++
    printf "[%s] n=%d conc=%d errors=%d\n", label, n, conc, err
    printf "  total : p50=%.1f p90=%.1f p95=%.1f p99=%.1f max=%.1f\n",
      q(tot,0.50), q(tot,0.90), q(tot,0.95), q(tot,0.99), tot[n]
    printf "  server: p50=%.1f p90=%.1f p95=%.1f p99=%.1f max=%.1f\n",
      q(srv,0.50), q(srv,0.90), q(srv,0.95), q(srv,0.99), srv[n]
  }' /tmp/lat.all.txt

#!/usr/bin/env bash
# Push resi-fanout ports into a local 3x-ui panel as Xray socks outbounds.
#
#   bash 3xui-push.sh --api http://127.0.0.1:7654 --key <API_KEY>
#
# What it does:
#   1. fetches the outbound snippet from the resi-fanout API
#   2. merges it into the panel database (settings.xrayTemplateConfig):
#      - outbounds with the same tag are replaced, new ones appended
#      - optional routing rules with --rule-inbound
#   3. restarts x-ui so Xray picks up the new outbounds
#
# Options:
#   --api URL            resi-fanout base url (default http://127.0.0.1:7654)
#   --key KEY            resi-fanout API key (if configured)
#   --ports 20001,20003  only these ports (default: all assigned)
#   --residential        only residential ports
#   --prefix resi        outbound tag prefix (default resi)
#   --rule-inbound T1,T2 add routing rule: these inbound tags -> --outbound tag
#   --outbound TAG       target outbound for --rule-inbound (default first port tag)
#   --db PATH            override panel db path
#   --no-restart         edit db but do not restart x-ui

set -euo pipefail

API="http://127.0.0.1:7654"
KEY=""
PORTS=""
RESIDENTIAL="0"
PREFIX="resi"
RULE_INBOUND=""
RULE_OUTBOUND=""
DB=""
RESTART="1"

log()  { printf '\033[1;34m[3xui-push]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --api)          API="${2:?}"; shift 2 ;;
    --key)          KEY="${2:?}"; shift 2 ;;
    --ports)        PORTS="${2:?}"; shift 2 ;;
    --residential)  RESIDENTIAL="1"; shift ;;
    --prefix)       PREFIX="${2:?}"; shift 2 ;;
    --rule-inbound) RULE_INBOUND="${2:?}"; shift 2 ;;
    --outbound)     RULE_OUTBOUND="${2:?}"; shift 2 ;;
    --db)           DB="${2:?}"; shift 2 ;;
    --no-restart)   RESTART="0"; shift ;;
    *) die "unknown option: $1" ;;
  esac
done

command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v curl >/dev/null 2>&1 || die "curl is required"

# ---------------------------------------------------------------- fetch snippet
Q="prefix=${PREFIX}"
[ -n "${PORTS}" ] && Q="${Q}&ports=${PORTS}"
[ "${RESIDENTIAL}" = "1" ] && Q="${Q}&residential=1"

TMP="$(mktemp -t resi-fanout-snippet.XXXXXX.json)"
trap 'rm -f "${TMP}"' EXIT
AUTH=()
[ -n "${KEY}" ] && AUTH=(-H "Authorization: Bearer ${KEY}")

log "fetching snippet: ${API}/api/3xui/snippet?${Q}"
curl -fsS "${AUTH[@]}" "${API}/api/3xui/snippet?${Q}" -o "${TMP}" \
  || die "cannot reach resi-fanout API — is the service running?"

# ---------------------------------------------------------------- locate panel db
if [ -z "${DB}" ]; then
  for cand in /etc/x-ui/x-ui.db /usr/local/x-ui/x-ui.db /usr/local/x-ui/bin/x-ui.db; do
    if [ -f "$cand" ]; then DB="$cand"; break; fi
  done
fi
[ -n "${DB}" ] && [ -f "${DB}" ] || die "3x-ui database not found (looked in /etc/x-ui, /usr/local/x-ui) — pass --db PATH"

log "panel db: ${DB}"
BACKUP="${DB}.bak.$(date +%Y%m%d%H%M%S)"
cp -a "${DB}" "${BACKUP}"
log "backup written: ${BACKUP}"

# ---------------------------------------------------------------- merge into db
RULE_INBOUND="${RULE_INBOUND}" RULE_OUTBOUND="${RULE_OUTBOUND}" python3 - "${DB}" "${TMP}" <<'PYEOF'
import json, os, sqlite3, sys

db_path, snip_path = sys.argv[1], sys.argv[2]
snippet = json.load(open(snip_path, encoding="utf-8"))
outbounds = snippet.get("outbounds", [])
if not outbounds:
    print("[error] snippet has no outbounds (no healthy/assigned ports?)", file=sys.stderr)
    sys.exit(1)

with open(db_path, "rb") as f:
    pass  # existence check only

conn = sqlite3.connect(db_path)
cur = conn.cursor()
row = cur.execute(
    "SELECT value FROM settings WHERE key = 'xrayTemplateConfig'"
).fetchone()

if row and str(row[0]).strip():
    tpl = json.loads(row[0])
else:
    print("[info] no template in db yet, using snippet.full_template as base")
    tpl = snippet["full_template"]

tpl.setdefault("outbounds", [])
tags = {o.get("tag") for o in tpl["outbounds"]}
new_tags = []
for o in outbounds:
    if o["tag"] in tags:
        tpl["outbounds"] = [o if x.get("tag") == o["tag"] else x for x in tpl["outbounds"]]
        print(f"[info] replaced existing outbound: {o['tag']}")
    else:
        tpl["outbounds"].append(o)
        new_tags.append(o["tag"])

# optional routing rule: inbound tags -> chosen outbound
rule_in = [t.strip() for t in os.environ.get("RULE_INBOUND", "").split(",") if t.strip()]
rule_out = os.environ.get("RULE_OUTBOUND", "")
if rule_in:
    if not rule_out:
        rule_out = snippet["outbounds"][0]["tag"]
    tpl.setdefault("routing", {}).setdefault("rules", [])
    rules = tpl["routing"]["rules"]
    tpl["routing"]["rules"] = [
        {"type": "field", "inboundTag": rule_in, "outboundTag": rule_out}
    ] + [r for r in rules if not (r.get("inboundTag") == rule_in)]
    print(f"[info] routing rule added: {rule_in} -> {rule_out}")

value = json.dumps(tpl, ensure_ascii=False)
if row:
    cur.execute("UPDATE settings SET value = ? WHERE key = 'xrayTemplateConfig'", (value,))
else:
    cur.execute("INSERT INTO settings (key, value) VALUES ('xrayTemplateConfig', ?)", (value,))
conn.commit()
conn.close()
print(f"[info] outbounds now: {len(tpl['outbounds'])}, added: {new_tags or 0}")
PYEOF

# ---------------------------------------------------------------- restart panel
if [ "${RESTART}" = "1" ]; then
  log "restarting x-ui"
  if command -v x-ui >/dev/null 2>&1; then
    x-ui restart >/dev/null 2>&1 || systemctl restart x-ui || true
  else
    systemctl restart x-ui || true
  fi
  log "done. check the panel: Xray 配置 → outbounds 里应出现 ${PREFIX}-<port> 出站"
else
  log "done (db updated, panel not restarted — restart x-ui manually)"
fi

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
#   --link-inbounds      instead of merging outbounds, clone a panel inbound
#                        per fanout port (fanout style; calls xui_db.py link)
#   --template-id N      inbound id used as the template for --link-inbounds
#   --host DOMAIN        host used in generated client links
#   --residential        only link residential exits
#   --unlink             remove previously linked inbounds (--link-inbounds off)
#   --no-restart         edit db but do not restart x-ui

set -euo pipefail

RF_CONF="${RF_CONF:-/etc/resi-fanout/config.json}"
API=""            # 为空时从 resi-fanout 配置自动推导
KEY="${RF_KEY:-}" # 用环境变量传入，避免出现在 ps 输出里
PORTS=""
RESIDENTIAL="0"
PREFIX="resi"
RULE_INBOUND=""
RULE_OUTBOUND=""
DB=""
RESTART="1"
LINK_INBOUNDS="0"
TEMPLATE_ID="1"
HOST="${XUI_HOST:-127.0.0.1}"
UNLINK="0"
XUI_DB_PY="${XUI_DB_PY:-/opt/resi-fanout/scripts/xui_db.py}"

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
    --link-inbounds) LINK_INBOUNDS="1"; shift ;;
    --template-id)  TEMPLATE_ID="${2:?}"; shift 2 ;;
    --host)         HOST="${2:?}"; shift 2 ;;
    --unlink)       UNLINK="1"; LINK_INBOUNDS="0"; shift ;;
    --no-restart)   RESTART="0"; shift ;;
    *) die "unknown option: $1" ;;
  esac
done

command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v curl >/dev/null 2>&1 || die "curl is required"

# ------------------------------------------------- auto-detect API endpoint/key
if [ -z "${API}" ] && [ -f "${RF_CONF}" ]; then
  # Emit plain assignments rather than `${VAR:-<value>}` expansions: a shell
  # parameter-expansion default is NOT re-parsed for quotes, so a value that
  # shlex.quote had to wrap ended up with literal quote characters inside it —
  # and an unset api_key became the two-character string `''`, which made the
  # script send `Authorization: Bearer ''` instead of omitting the header.
  eval "$(python3 - "${RF_CONF}" <<'PYEOF2'
import json, shlex, sys
cfg = json.load(open(sys.argv[1]))["server"]
host, _, port = cfg["listen"].rpartition(":")
scheme = "https" if cfg.get("tls", {}).get("enabled") else "http"
base = cfg.get("base_path", "").rstrip("/")
print(f'CONF_API={shlex.quote(scheme + "://127.0.0.1:" + port + base)}')
print(f'CONF_KEY={shlex.quote(cfg.get("api_key", "") or "")}')
PYEOF2
)"
  # ${CONF_API:-} / ${CONF_KEY:-} rather than ${CONF_API}: if the python above
  # exits non-zero (truncated/corrupt config, unreadable file, an old python3)
  # the eval consumes nothing, the variables are never assigned, and `set -u`
  # then aborts with a bare "CONF_API: unbound variable" — which also made the
  # deliberate `|| API="http://127.0.0.1:7654"` fallback below unreachable, so a
  # corrupt config looked identical to an unreachable API.
  API="${1:-${CONF_API:-}}"
  KEY="${RF_KEY:-${CONF_KEY:-}}"
  unset CONF_API CONF_KEY
  log "自动读取到 API: ${API}"
fi
[ -n "${API}" ] || API="http://127.0.0.1:7654"

# ---------------------------------------------------------------- fetch snippet
Q="prefix=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "${PREFIX}")"
[ -n "${PORTS}" ] && Q="${Q}&ports=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "${PORTS}")"
[ "${RESIDENTIAL}" = "1" ] && Q="${Q}&residential=1"

TMP="$(mktemp -t resi-fanout-snippet.XXXXXX.json)"
trap 'rm -f "${TMP}"' EXIT
AUTH=()
[ -n "${KEY}" ] && AUTH=(-H "Authorization: Bearer ${KEY}")

log "fetching snippet: ${API}/api/3xui/snippet?${Q}"
# ${AUTH[@]+"${AUTH[@]}"} rather than "${AUTH[@]}": expanding an empty array
# under `set -u` is an "unbound variable" error on bash < 4.4, which still
# ships on CentOS 7 — a supported target per install.sh's header.
curl -fsS ${AUTH[@]+"${AUTH[@]}"} "${API}/api/3xui/snippet?${Q}" -o "${TMP}" \
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
# The panel may be running in WAL mode, where a bare `cp` of the main file
# silently misses everything still in the -wal sidecar — i.e. it produces a
# backup that is not the database. VACUUM INTO exports a consistent snapshot
# (same approach, and same reason, as scripts/xui_db.py).
if ! python3 - "${DB}" "${BACKUP}" <<'PYBAK'
import os, sqlite3, sys

db, bak = sys.argv[1], sys.argv[2]
# sqlite3's own backup API, for the same reasons as scripts/xui_db.py: a bare
# copy of the main file misses every uncheckpointed -wal transaction, so the
# "backup" is not the database; and VACUUM INTO needs SQLite >= 3.27 (CentOS 7
# has 3.7.17) and fails outright when the target already exists — which it does
# on a same-second rerun, because the name has only second resolution. The old
# code swallowed both and degraded to copy2.
try:
    if os.path.exists(bak):
        os.unlink(bak)
    dst = sqlite3.connect(bak)
    try:
        sqlite3.connect(db).backup(dst)
    finally:
        dst.close()
except (sqlite3.Error, OSError) as exc:
    print(f"backup failed: {exc}", file=sys.stderr)
    sys.exit(1)
if not os.path.exists(bak) or os.path.getsize(bak) == 0:
    print("backup is empty", file=sys.stderr)
    sys.exit(1)
PYBAK
then
  die "cannot create a usable backup of ${DB} — refusing to modify the panel database"
fi
log "backup written: ${BACKUP}"

# ---------------------------------------------------------------- inbound linking
if [ "${LINK_INBOUNDS}" = "1" ] || [ "${UNLINK}" = "1" ]; then
  [ -f "${XUI_DB_PY}" ] || die "xui_db.py not found at ${XUI_DB_PY} (override with XUI_DB_PY=...)"
  if [ "${UNLINK}" = "1" ]; then
    log "removing previously linked inbounds from the panel"
    python3 "${XUI_DB_PY}" unlink --db "${DB}" \
      --inbound-prefix "resi-in-" --outbound-prefix "${PREFIX}" \
      | python3 -m json.tool
  else
    ENTRIES="$(curl -fsS ${AUTH[@]+"${AUTH[@]}"} "${API}/api/ports" | RESIDENTIAL="${RESIDENTIAL}" python3 -c '
import json, os, sys
d = json.load(sys.stdin)
only_resi = os.environ.get("RESIDENTIAL") == "1"
out = [
    {"port": e["port"], "country": e.get("country_code") or "xx",
     "residential": bool(e.get("residential")), "kind": e.get("kind", "proxy")}
    for e in d.get("items", [])
    if not only_resi or e.get("residential")
]
print(json.dumps(out))')"
    CNT="$(printf '%s' "${ENTRIES}" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
    [ "${CNT}" != "0" ] || die "no fanout ports to link (is resi-fanout running and ports assigned?)"
    log "linking ${CNT} fanout ports to panel inbounds (template #${TEMPLATE_ID}, host ${HOST})"
    python3 "${XUI_DB_PY}" link --db "${DB}" \
      --template-id "${TEMPLATE_ID}" \
      --entries "${ENTRIES}" \
      --host "${HOST}" \
      --inbound-prefix "resi-in-" \
      --outbound-prefix "${PREFIX}" \
      --inbound-port-base "${XUI_INBOUND_PORT_BASE:-31000}" \
      | python3 -m json.tool
  fi
  if [ "${RESTART}" = "1" ]; then
    log "restarting x-ui"
    if command -v x-ui >/dev/null 2>&1; then
      x-ui restart >/dev/null 2>&1 || systemctl restart x-ui || die "面板重启失败：配置已写入数据库但 x-ui 未重启，请手动执行 systemctl restart x-ui"
    else
      systemctl restart x-ui || die "面板重启失败：配置已写入数据库但 x-ui 未重启，请手动执行 systemctl restart x-ui"
    fi
  fi
  log "done"
  exit 0
fi

# ---------------------------------------------------------------- merge outbounds
RULE_INBOUND="${RULE_INBOUND}" RULE_OUTBOUND="${RULE_OUTBOUND}" python3 - "${DB}" "${TMP}" <<'PYEOF'
import json, os, sqlite3, sys

db_path, snip_path = sys.argv[1], sys.argv[2]
if not os.path.exists(db_path):
    print(f"[error] panel database not found: {db_path}", file=sys.stderr)
    sys.exit(1)
snippet = json.load(open(snip_path, encoding="utf-8"))
outbounds = snippet.get("outbounds", [])
if not outbounds:
    print("[error] snippet has no outbounds (no healthy/assigned ports?)", file=sys.stderr)
    sys.exit(1)

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
    # Drop every existing rule that routes ANY of our tags somewhere else, not
    # just one whose inboundTag list matches exactly. With exact matching,
    # re-running with an overlapping set (e.g. "vmess-in,trojan-in" after
    # "vmess-in") left the old rule in place and Xray then had two conflicting
    # rules for the same inbound.
    want = set(rule_in)
    kept = []
    for r in rules:
        tags = r.get("inboundTag")
        tags = set(tags) if isinstance(tags, list) else ({tags} if tags else set())
        if tags & want:
            continue
        kept.append(r)
    tpl["routing"]["rules"] = [
        {"type": "field", "inboundTag": rule_in, "outboundTag": rule_out}
    ] + kept
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
    x-ui restart >/dev/null 2>&1 || systemctl restart x-ui || die "面板重启失败，请手动执行 systemctl restart x-ui"
  else
    systemctl restart x-ui || die "面板重启失败，请手动执行 systemctl restart x-ui"
  fi
  log "done. check the panel: Xray 配置 → outbounds 里应出现 ${PREFIX}-<port> 出站"
else
  log "done (db updated, panel not restarted — restart x-ui manually)"
fi

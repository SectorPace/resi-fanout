#!/usr/bin/env python3
"""resi-fanout ↔ 3x-ui glue.

Reads/writes the 3x-ui panel database (x-ui.db, SQLite) to link every
fanout port to its own panel inbound, the same way byJoey/fanout hands its
exits to 3x-ui.

Schema-adaptive on purpose: 3x-ui v2 keeps clients inline inside the
inbound's settings JSON, v3.x moved them into a separate table, and column
names differ slightly between versions — so everything is discovered via
PRAGMA instead of hardcoded.

Subcommands (all print one JSON object to stdout):
  list     --db PATH
  preview  --db PATH --template-id N --entries JSON [opts]
  link     --db PATH --template-id N --entries JSON [opts] [--dry-run 1]
  unlink   --db PATH --inbound-prefix resi-in-

Entries JSON: [{"port": 21000, "country": "JP", "residential": true}, ...]
"""

import argparse
import base64
import json
import os
import re
import sqlite3
import sys
import time
import urllib.parse


def die(msg, code=1):
    print(json.dumps({"ok": False, "error": msg}, ensure_ascii=False))
    sys.exit(code)


def ok(**kw):
    out = {"ok": True}
    out.update(kw)
    print(json.dumps(out, ensure_ascii=False))
    sys.exit(0)


def snapshot(conn, db_path, backup):
    """Write a *consistent* copy of the panel database to `backup`.

    Why not the obvious two options:

    * `shutil.copy2(db_path, backup)` — on a WAL database the main file alone
      predates every uncheckpointed transaction, so the "backup" is missing
      committed rows. It was the old fallback, i.e. the guarantee that exists so
      a failed write is recoverable silently produced something that is not.
    * `VACUUM INTO` — a correct snapshot, but it only exists from SQLite 3.27
      (CentOS 7 ships 3.7.17, which install.sh claims to support) and it raises
      `output file already exists` when the target is present. The backup name
      has only second resolution, so two runs in the same second degrade
      silently. Both cases were swallowed by a bare `except sqlite3.Error`,
      after which the failure message advertised the broken file as the
      recovery point.

    `Connection.backup` is available on every supported SQLite, includes WAL
    content, and gives a consistent snapshot of a live database. A pre-existing
    target is removed first so a same-second rerun cannot half-overwrite. If the
    snapshot cannot be taken we abort: continuing would mutate the panel with no
    way back.
    """
    try:
        if os.path.exists(backup):
            os.unlink(backup)
        dst = sqlite3.connect(backup)
        try:
            conn.backup(dst)
        finally:
            dst.close()
    except (sqlite3.Error, OSError) as exc:
        die(f"panel database backup failed, refusing to modify {db_path}: {exc}")
    if not os.path.exists(backup) or os.path.getsize(backup) == 0:
        die(f"panel database backup is empty, refusing to modify {db_path}")


def columns(conn, table):
    cur = conn.execute(f"PRAGMA table_info({table})")
    return [r[1] for r in cur.fetchall()]


def table_exists(conn, name):
    cur = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (name,)
    )
    return cur.fetchone() is not None


def find_clients_table(conn):
    """v3 keeps clients in a side table referencing inbounds.id."""
    for name in ("clients", "client_traffics", "client_traffic"):
        if table_exists(conn, name):
            cols = columns(conn, name)
            if "inbound_id" in cols:
                return name, cols
    return None, []


def load_template(conn, template_id):
    cols = columns(conn, "inbounds")
    if not cols:
        die("inbounds table not found — is this a 3x-ui database?")
    cur = conn.execute("SELECT * FROM inbounds WHERE id=?", (template_id,))
    row = cur.fetchone()
    if row is None:
        die(f"template inbound {template_id} not found")
    return dict(zip(cols, row))


def settings_json(row):
    try:
        return json.loads(row.get("settings") or "{}")
    except (ValueError, TypeError):
        return {}


def stream_json(row):
    try:
        return json.loads(row.get("stream_settings") or "{}")
    except (ValueError, TypeError):
        return {}


def inline_clients(row):
    s = settings_json(row)
    clients = s.get("clients") or []
    return clients if isinstance(clients, list) else []


def build_link(row, port, remark, host):
    """Best-effort client link from the template inbound's own settings."""
    proto = (row.get("protocol") or "vless").lower()
    st = stream_json(row)
    network = st.get("network") or "tcp"
    security = st.get("security") or "none"
    reality = (st.get("realitySettings") or {})
    tls = st.get("tlsSettings") or {}
    ws = st.get("wsSettings") or {}
    grpc = st.get("grpcSettings") or {}
    sni = reality.get("serverNames") or tls.get("serverName") or ""
    if isinstance(sni, list):
        sni = sni[0] if sni else ""
    params = {"type": network, "security": security}
    if network == "ws":
        if ws.get("path"):
            params["path"] = ws["path"]
        if (ws.get("headers") or {}).get("Host"):
            params["host"] = ws["headers"]["Host"]
    elif network == "grpc":
        if grpc.get("serviceName"):
            params["serviceName"] = grpc["serviceName"]
    if security == "reality" and reality.get("publicKey"):
        params["pbk"] = reality["publicKey"]
        # `dict.get(key, default)` only applies the default when the key is
        # ABSENT, so `"shortIds": []` (a realistic state after an operator clears
        # the field in the panel) indexed an empty list and killed the whole
        # run with IndexError -- even though build_link documents itself as
        # best-effort. `or` also covers a present-but-empty value. The sni
        # handling above already guards the same way.
        short_ids = reality.get("shortIds")
        params["fp"] = (
            short_ids[0]
            if isinstance(short_ids, list) and short_ids
            else "chrome"
        )
    if sni:
        params["sni"] = sni

    clients = inline_clients(row)
    client = clients[0] if clients else {}
    flow = client.get("flow") or (st.get("sockopt") or {}).get("xraySetting", {}).get("flow")
    if flow:
        params["flow"] = flow

    if proto == "vless":
        q = dict(params)
        q["encryption"] = "none"
        uid = client.get("id") or "00000000-0000-0000-0000-000000000000"
        return f"vless://{uid}@{host}:{port}?{urllib.parse.urlencode(q)}#{urllib.parse.quote(remark)}"
    if proto == "trojan":
        pw = client.get("password") or ""
        return f"trojan://{urllib.parse.quote(pw, safe='')}@{host}:{port}?{urllib.parse.urlencode(params)}#{urllib.parse.quote(remark)}"
    if proto == "shadowsocks":
        pw = client.get("password") or ""
        method = client.get("method") or "aes-256-gcm"
        userinfo = base64.urlsafe_b64encode(f"{method}:{pw}".encode()).decode().rstrip("=")
        return f"ss://{userinfo}@{host}:{port}#{urllib.parse.quote(remark)}"
    if proto in ("socks", "http"):
        return f"{proto}://{host}:{port}#{urllib.parse.quote(remark)}"
    # vmess is not guessable without a full payload; point at the panel
    return f"{proto}://{host}:{port} (see panel for the full link)"


def list_inbounds(conn):
    cols = columns(conn, "inbounds")
    if "id" not in cols:
        # A wrong --db used to build `SELECT  FROM inbounds` (empty column list)
        # and surface as `near "FROM": syntax error`. Say what is actually wrong.
        found = [
            r[0]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            ).fetchall()
        ]
        die(
            "no `inbounds` table with an `id` column in this database "
            f"(tables: {', '.join(found) or 'none'}) — is this a 3x-ui panel database?"
        )
    want = [c for c in ("id", "tag", "remark", "port", "protocol", "enable") if c in cols]
    rows = conn.execute(f"SELECT {','.join(want)} FROM inbounds ORDER BY id").fetchall()
    clients_tbl, client_cols = find_clients_table(conn)
    out = []
    for r in rows:
        d = dict(zip(want, r))
        full = conn.execute(
            f"SELECT settings FROM inbounds WHERE id=?", (d["id"],)
        ).fetchone()
        n = len(inline_clients({"settings": full[0] if full else ""}))
        if clients_tbl and "inbound_id" in client_cols:
            n = conn.execute(
                f"SELECT COUNT(*) FROM {clients_tbl} WHERE inbound_id=?", (d["id"],)
            ).fetchone()[0]
        d["clients"] = n
        d["enable"] = bool(d.get("enable"))
        out.append(d)
    return {
        "inbounds": out,
        "clients_table": clients_tbl,
        "has_tag": "tag" in cols,
        "columns": cols,
    }


def merge_template(conn, entries, outbound_prefix, inbound_prefix, fanout_bind):
    """Merge socks outbounds + inbound→outbound rules into the Xray template."""
    key = "xrayTemplateConfig"
    row = conn.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
    if row and str(row[0]).strip():
        tpl = json.loads(row[0])
    else:
        die("panel settings has no xrayTemplateConfig — open the panel once first")

    tpl.setdefault("outbounds", [])
    tpl.setdefault("routing", {}).setdefault("rules", [])

    made_outbounds, made_rules = [], []
    for e in entries:
        fanout_port = e.get("fanout_port", e.get("port"))
        # 用 plan 里的实际 tag：do_link 可能为避免重名给入站加了后缀，
        # 若在这里重算 tag，路由规则就会指向一个不存在的入站（静默直连）
        out_tag = e.get("outbound_tag") or f"{outbound_prefix}-{fanout_port}"
        in_tag = e.get("inbound_tag") or f"{inbound_prefix}{fanout_port}"
        ob = {
            "tag": out_tag,
            "protocol": "socks",
            "settings": {"servers": [{"address": fanout_bind, "port": fanout_port}]},
        }
        tpl["outbounds"] = [ob if o.get("tag") == out_tag else o for o in tpl["outbounds"]]
        if not any(o.get("tag") == out_tag for o in tpl["outbounds"]):
            tpl["outbounds"].append(ob)
        rule = {"type": "field", "inboundTag": [in_tag], "outboundTag": out_tag}
        tpl["routing"]["rules"] = [
            rule
            if r.get("inboundTag") == [in_tag]
            else r
            for r in tpl["routing"]["rules"]
        ]
        if not any(r.get("inboundTag") == [in_tag] for r in tpl["routing"]["rules"]):
            tpl["routing"]["rules"].append(rule)
        made_outbounds.append(out_tag)
        made_rules.append(in_tag)

    value = json.dumps(tpl, ensure_ascii=False)
    if row:
        conn.execute("UPDATE settings SET value=? WHERE key=?", (value, key))
    else:
        conn.execute("INSERT INTO settings (key, value) VALUES (?,?)", (key, value))
    return made_outbounds, made_rules


def do_link(args):
    entries = json.loads(args.entries)
    if not entries:
        die("no entries")
    conn = sqlite3.connect(args.db, timeout=30)
    conn.row_factory = sqlite3.Row
    template = load_template(conn, args.template_id)
    cols = columns(conn, "inbounds")
    clients_tbl, client_cols = find_clients_table(conn)

    used_ports = {
        r[0]
        for r in conn.execute("SELECT port FROM inbounds").fetchall()
        if r and r[0]
    }
    if "tag" in cols:
        used_tags = {
            r[0] for r in conn.execute("SELECT tag FROM inbounds WHERE tag IS NOT NULL").fetchall()
        }
    else:
        used_tags = set()

    plan, next_port = [], args.inbound_port_base
    for e in entries:
        # `entries` comes from --entries (caller-supplied JSON), so a missing
        # "port" used to raise a bare KeyError traceback on stderr — this tool
        # documents that every subcommand prints one JSON object to stdout.
        port = e.get("port")
        if not isinstance(port, int) or not (1 <= port <= 65535):
            die(f"entry is missing a usable 'port': {json.dumps(e, ensure_ascii=False)}")
        in_tag = f"{args.inbound_prefix}{port}"
        cc = (e.get("country") or "xx").upper()
        kind = "住宅" if e.get("residential") else "机房"
        remark = f"resi-{cc}-{port}({kind})"
        while next_port in used_ports:
            next_port += 1
        # Same bound the *fanout* port above already enforces. Without it a
        # --inbound-port-base of 70000 (or -5) was written straight into
        # inbounds.port and the panel then mis-bound or rejected it.
        if not (1 <= next_port <= 65535):
            die(
                f"ran out of usable inbound ports at {next_port} "
                f"(base {args.inbound_port_base}); lower --inbound-port-base"
            )
        used_ports.add(next_port)
        plan.append(
            {
                "fanout_port": port,
                "inbound_port": next_port,
                "inbound_tag": in_tag,
                "outbound_tag": f"{args.outbound_prefix}-{port}",
                "remark": remark,
                "link": build_link(template, next_port, remark, args.host),
            }
        )
        next_port += 1

    if args.dry_run:
        conn.close()
        return ok(plan=plan, template=template.get("remark") or template.get("tag"))

    backup = f"{args.db}.bak.{time.strftime('%Y%m%d%H%M%S')}"
    snapshot(conn, args.db, backup)

    created = []
    try:
        for p in plan:
            values = dict(template)
            values.pop("id", None)
            for k, v in (
                ("port", p["inbound_port"]),
                ("tag", p["inbound_tag"]),
                ("remark", p["remark"]),
                ("enable", 1),
                ("up", 0),
                ("down", 0),
                ("total", 0),
            ):
                if k in values:
                    values[k] = v
            # keep tags unique even if a stale row exists
            while p["inbound_tag"] in used_tags:
                p["inbound_tag"] = p["inbound_tag"] + "x"
                values["tag"] = p["inbound_tag"]
            used_tags.add(p["inbound_tag"])

            keys = list(values.keys())
            ph = ",".join("?" for _ in keys)
            cur = conn.execute(
                f"INSERT INTO inbounds ({','.join(keys)}) VALUES ({ph})",
                [values[k] for k in keys],
            )
            new_id = cur.lastrowid
            # v3: duplicate the template's clients for the new inbound
            if clients_tbl and "inbound_id" in client_cols:
                for cr in conn.execute(
                    f"SELECT * FROM {clients_tbl} WHERE inbound_id=?",
                    (args.template_id,),
                ).fetchall():
                    cdict = dict(cr)
                    cdict.pop("id", None)
                    cdict["inbound_id"] = new_id
                    for k in ("up", "down", "total"):
                        if k in cdict:
                            cdict[k] = 0
                    ckeys = list(cdict.keys())
                    conn.execute(
                        f"INSERT INTO {clients_tbl} ({','.join(ckeys)}) "
                        f"VALUES ({','.join('?' for _ in ckeys)})",
                        [cdict[k] for k in ckeys],
                    )
            created.append({**p, "inbound_id": new_id})

        outs, rules = merge_template(
            conn, plan, args.outbound_prefix, args.inbound_prefix, args.fanout_bind
        )
        conn.commit()
    except Exception as exc:  # noqa: BLE001
        conn.rollback()
        die(f"write failed (db untouched, backup at {backup}): {exc}")
    finally:
        conn.close()
    ok(created=created, backup=backup, outbounds=outs, rules=rules)


def do_unlink(args):
    conn = sqlite3.connect(args.db, timeout=30)
    conn.row_factory = sqlite3.Row
    cols = columns(conn, "inbounds")
    clients_tbl, client_cols = find_clients_table(conn)
    if "tag" not in cols:
        die("panel has no tag column, cannot identify managed inbounds")

    # LIKE 通配符转义，避免 resi_ 之类的前缀误伤其它入站
    pattern = (
        args.inbound_prefix.replace("\\", "\\\\")
        .replace("%", "\\%")
        .replace("_", "\\_")
    ) + "%"
    rows = conn.execute(
        "SELECT id, tag, port FROM inbounds WHERE tag LIKE ? ESCAPE '\\'",
        (pattern,),
    ).fetchall()
    if not rows:
        conn.close()
        return ok(removed=[])

    # 与 do_link 一致：先备份，任何失败都不留下写坏的面板库
    backup = f"{args.db}.bak.{time.strftime('%Y%m%d%H%M%S')}"
    snapshot(conn, args.db, backup)

    removed_tags = [r["tag"] for r in rows]
    ids = [r["id"] for r in rows]
    try:
        for i in ids:
            if clients_tbl and "inbound_id" in client_cols:
                conn.execute(f"DELETE FROM {clients_tbl} WHERE inbound_id=?", (i,))
            conn.execute("DELETE FROM inbounds WHERE id=?", (i,))

        key = "xrayTemplateConfig"
        row = conn.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
        if row and str(row[0]).strip():
            tpl = json.loads(row[0])
            # 我们创建的出站 tag 形如 resi-20000（纯数字端口），
            # 入站 tag 可能带去重后缀(resi-in-20000x)，所以按数字端口匹配出站
            port_of = {
                int(m.group(1))
                for t in removed_tags
                for m in [re.fullmatch(re.escape(args.inbound_prefix) + r"(\d+)x*", t)]
                if m
            }
            gone_out = {f"{args.outbound_prefix}-{p}" for p in port_of}
            tpl["outbounds"] = [
                o for o in tpl.get("outbounds", []) if o.get("tag") not in gone_out
            ]

            def keep_rule(r):
                tags = r.get("inboundTag")
                tags = set(tags) if isinstance(tags, list) else ({tags} if tags else set())
                return not (tags & set(removed_tags)) and r.get("outboundTag") not in gone_out

            tpl.setdefault("routing", {})["rules"] = [
                r for r in tpl.get("routing", {}).get("rules", []) if keep_rule(r)
            ]
            conn.execute(
                "UPDATE settings SET value=? WHERE key=?", (json.dumps(tpl, ensure_ascii=False), key)
            )
        conn.commit()
    except Exception as exc:  # noqa: BLE001
        conn.rollback()
        conn.close()
        die(f"解绑失败（面板库已回滚，备份在 {backup}）：{exc}")
    conn.close()
    ok(removed=[{"id": r["id"], "tag": r["tag"], "port": r["port"]} for r in rows], backup=backup)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["list", "preview", "link", "unlink"])
    ap.add_argument("--db", required=True)
    ap.add_argument("--template-id", type=int, default=0)
    ap.add_argument("--entries", default="[]")
    ap.add_argument("--host", default=os.environ.get("XUI_HOST", "127.0.0.1"))
    ap.add_argument("--inbound-prefix", default="resi-in-")
    ap.add_argument("--outbound-prefix", default="resi")
    ap.add_argument("--inbound-port-base", type=int, default=31000)
    ap.add_argument("--fanout-bind", default="127.0.0.1")
    ap.add_argument("--dry-run", type=int, default=0)
    args = ap.parse_args()

    if not os.path.exists(args.db):
        die(f"panel database not found: {args.db}")

    if args.cmd == "list":
        conn = sqlite3.connect(args.db, timeout=30)
        try:
            ok(**list_inbounds(conn))
        finally:
            # unreachable via ok() (it exits), but correct if the call ever
            # returns instead
            conn.close()
    elif args.cmd in ("preview", "link"):
        if args.cmd == "preview":
            args.dry_run = 1  # preview never writes
        do_link(args)
    elif args.cmd == "unlink":
        do_unlink(args)


if __name__ == "__main__":
    # Every subcommand documents "prints one JSON object to stdout", and
    # 3xui-push.sh pipes that into `python3 -m json.tool`. Without this wrapper
    # any sqlite3/json error escaped as a raw traceback on stderr with nothing
    # on stdout — e.g. `list --db <wrong-file>` builds `SELECT  FROM inbounds`
    # (empty column list) and dies with a syntax error, and `link --entries
    # 'not-json'` raised an uncaught JSONDecodeError. Both are ordinary operator
    # mistakes, so they must come back as a readable message, not a stack trace.
    try:
        main()
    except SystemExit:
        raise
    except BaseException as exc:  # noqa: BLE001
        die(f"{type(exc).__name__}: {exc}")
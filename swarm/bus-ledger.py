"""Ledger of the work the orchestrator handed out on the bus.

Reads the bus log as JSON lines on standard input (sommelier log --since 1 --limit 1000 --json) and
prints one line per thing that needs the orchestrator:

  OVERDUE <key> : a TASK or RUN_REQUEST nobody answered, or accepted and not delivered in time
  IDLE <key>    : a participant who was active lately and has nothing open
  OVERDUE gone-<who>-<hour> : silent for 45 minutes with open items or a declared capacity
  (nothing)     : all is moving

With --table it prints every open item instead, for a look by hand.

Rules: an offer counts as answered by ACCEPT or DECLINE that refers to it, as delivered by READY,
RUN_RESULT, FIX, RELEASE or STUCK that refers to it (or to the ACCEPT), or by a later message of
the same sender on the same topic of one of those kinds. A participant whose quota is used up
(quotas.tsv, column "back at" in the future) is left alone until then.
"""
import json, os, sys, time, calendar

UNANSWERED_MIN = 12
DELIVER_MIN = {"RUN_REQUEST": 45, "TASK": 100}
GONE_MIN = 45          # silent this long (after the polls) and the participant counts as gone
IDLE_MIN = 25          # no open item and no message for this long, but active in the last 3 hours
ME = os.environ.get("SOMMELIER_ORCH", "owner")   # the orchestrator's name on the bus
IGNORE = {"codex", "watch"}   # no participants: a revoked name and the dashboard observer
DONE = {"READY", "RUN_RESULT", "FIX", "RELEASE", "STUCK", "LANDED"}


def ts(s):
    return calendar.timegm(time.strptime(s[:19], "%Y-%m-%dT%H:%M:%S"))


def main():
    table = "--table" in sys.argv
    now = time.time()
    msgs = []
    for line in sys.stdin:
        line = line.strip()
        if line.startswith("{"):
            try:
                msgs.append(json.loads(line))
            except ValueError:
                pass
    quota_until = {}
    data = os.environ.get("SOMMELIER_DATA") or os.path.join(os.environ.get("LOCALAPPDATA") or os.path.expanduser("~/.local/share"), "sommelier")
    q = os.environ.get("SOMMELIER_QUOTAS") or os.path.join(data, "quotas.tsv")
    if os.path.exists(q):
        for row in open(q, encoding="utf-8").read().splitlines()[1:]:
            c = row.split("\t")
            if len(c) >= 4:
                try:
                    quota_until[c[0]] = max(quota_until.get(c[0], 0), calendar.timegm(time.strptime(c[3][:16], "%Y-%m-%d %H:%M")))
                except ValueError:
                    pass

    by_id = {m["id"]: m for m in msgs}
    refs_to = {}
    for m in msgs:
        for r in m.get("refs") or []:
            if r.startswith("#") and r[1:].isdigit():
                refs_to.setdefault(int(r[1:]), []).append(m)

    last_seen, open_for = {}, {}
    out = []
    for m in msgs:
        if m["from"] != ME:
            last_seen[m["from"]] = max(last_seen.get(m["from"], 0), ts(m["ts"]))
    for m in msgs:
        if m["from"] != ME or m["kind"] not in DELIVER_MIN:
            continue
        to = m.get("to") or []
        if not to:
            continue
        sent = ts(m["ts"])
        for who in to:
            answers = [a for a in refs_to.get(m["id"], []) if a["from"] == who]
            later = [a for a in msgs if a["from"] == who and a["id"] > m["id"] and a["kind"] in DONE and (a.get("topic") == m.get("topic") or str(m["id"]) in (a.get("topic") or ""))]
            accept = next((a for a in answers if a["kind"] in ("ACCEPT", "CHAT", "CLAIM")), None)  # a reply that refers to the offer counts as taking it up
            declined = any(a["kind"] == "DECLINE" for a in answers) and not accept
            done = any(a["kind"] in DONE for a in answers) or bool(later)
            if accept:
                done = done or any(a["kind"] in DONE for a in refs_to.get(accept["id"], []))
            # The sender took the offer back or over: a later message of the sender naming it by #id.
            if any(a["from"] == ME and a["id"] > m["id"] for a in refs_to.get(m["id"], [])):
                done = True
            if done or declined:
                state = "declined" if declined and not done else "done"
            elif accept:
                state = "accepted"
                open_for.setdefault(who, []).append(m["id"])
            else:
                state = "unanswered"
                open_for.setdefault(who, []).append(m["id"])
            age = (now - (ts(accept["ts"]) if accept else sent)) / 60
            head = m["body"][:90].replace("\n", " ")
            if table:
                print(f"#{m['id']} {m['kind']} to {who} [{m.get('topic','')}] {state}, {age:.0f} min : {head}")
                continue
            if quota_until.get(who, 0) > now:
                continue
            if state == "unanswered" and age > UNANSWERED_MIN:
                out.append(f"OVERDUE unanswered-{m['id']}-{who}-{int(age // 30)} : #{m['id']} {m['kind']} to {who} [{m.get('topic','')}] has no ACCEPT or DECLINE after {age:.0f} min : {head}")
            elif state == "accepted" and age > DELIVER_MIN[m["kind"]]:
                out.append(f"OVERDUE late-{m['id']}-{who}-{int(age // 30)} : #{m['id']} {m['kind']} accepted by {who} {age:.0f} min ago, nothing delivered [{m.get('topic','')}] : {head}")
            elif state == "declined" and not table:
                d = next(a for a in answers if a["kind"] == "DECLINE")
                out.append(f"OVERDUE declined-{m['id']}-{who} : #{m['id']} {m['kind']} was DECLINED by {who}: {d['body'][:160]} : give it to someone else or take it over")
    if not table:
        for who, seen in last_seen.items():
            quiet = (now - seen) / 60
            if who in IGNORE or who in open_for or quota_until.get(who, 0) > now or quiet > 180 or quiet < IDLE_MIN:
                continue
            out.append(f"IDLE idle-{who}-{int(quiet // 30)} : {who} has nothing open from us and sent nothing for {quiet:.0f} min: hand out work or ask")
        # A participant that talks on the bus but whose status rows are all old: the dashboard then
        # shows nothing true about it. Rows come from `sommelier status list --json` (file in BUS_STATUS).
        newest, declared = {}, {}
        sp = os.environ.get("BUS_STATUS", "")
        if sp and os.path.exists(sp):
            newest = {}
            for line in open(sp, encoding="utf-8", errors="replace"):
                line = line.strip()
                if line.startswith("{"):
                    try:
                        r = json.loads(line)
                        newest[r["owner"]] = max(newest.get(r["owner"], 0), ts(r["ts"]))
                    except (ValueError, KeyError):
                        pass
            for who, seen in last_seen.items():
                if who in IGNORE or (now - seen) / 60 > 20:
                    continue
                age = (now - newest[who]) / 60 if who in newest else None
                if age is None:
                    out.append(f"IDLE nostatus-{who}-{int(now // 3600)} : {who} is sending messages but has never published a status row: ask for `sommelier status set`")
                elif age > 25:
                    out.append(f"IDLE oldstatus-{who}-{int(now // 1800)} : {who} is sending messages but its newest status row is {age:.0f} min old: ask it to keep its status current")
        # Silence: a participant (with a token, not on a quota break) that sent nothing and published
        # no status row for 15 minutes gets a poll, one every 15 minutes, sent by the watchdog itself.
        if sp and os.path.exists(sp):
            for who in set(list(last_seen) + list(newest)):
                if who in IGNORE or who == ME or quota_until.get(who, 0) > now:
                    continue
                heard = max(last_seen.get(who, 0), newest.get(who, 0))
                q = (now - heard) / 60
                if 15 <= q < 24 * 60:
                    out.append(f"POLL {who} {int(now // 900)} : {q:.0f}")
        # Declared capacity against busy workers: a participant that promised N agents in parallel
        # (capacity.tsv, first number of column 2, newest line wins) and has fewer fresh busy rows.
        cap_file = os.path.join(os.environ.get("SOMMELIER_DATA") or os.path.join(os.environ.get("LOCALAPPDATA") or os.path.expanduser("~/.local/share"), "sommelier"), "capacity.tsv")
        if sp and os.path.exists(sp) and os.path.exists(cap_file):
            import re
            declared = {}
            for row in open(cap_file, encoding="utf-8").read().splitlines()[1:]:
                c = row.split("\t")
                n = re.search(r"\d+", c[1]) if len(c) > 1 else None
                if n and c[0] != ME:
                    declared[c[0]] = int(n.group())
            busy = {}
            for line in open(sp, encoding="utf-8", errors="replace"):
                line = line.strip()
                if line.startswith("{"):
                    try:
                        r = json.loads(line)
                    except ValueError:
                        continue
                    if r.get("state") in ("running", "blocked") and (now - ts(r["ts"])) / 60 < 20:
                        busy[r["owner"]] = busy.get(r["owner"], 0) + 1
            for who, n in declared.items():
                if quota_until.get(who, 0) > now:
                    continue
                b = busy.get(who, 0)
                if b < n:
                    out.append(f"IDLE underused-{who}-{b}-{n}-{int(now // 1800)} : {who} promised {n} agents in parallel and has {b} busy: send {n - b} more pieces of work")
        # Gone: a participant that is not on a quota break, has open items from us or a declared
        # capacity, and has been silent (no message, no status row) for 45 minutes or more, although
        # the polls went out. It counts as gone: its open items are handed on. One line per hour slot.
        for who in set(list(last_seen) + list(newest)):
            if who in IGNORE or who == ME or quota_until.get(who, 0) > now:
                continue
            if who not in open_for and who not in declared:
                continue
            heard = max(last_seen.get(who, 0), newest.get(who, 0))
            q = (now - heard) / 60
            if GONE_MIN <= q < 24 * 60:
                ids = " ".join(f"#{i}" for i in open_for.get(who, [])) or "(none from us, hand on its declared capacity)"
                out.append(f"OVERDUE gone-{who}-{int(now // 3600)} : {who} is silent for {q:.0f} min and did not answer the polls: treat it as gone and hand on its open items: {ids}")
        if out:
            print("\n".join(out))


main()

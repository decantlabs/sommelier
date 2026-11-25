#!/usr/bin/env python3
"""Compare two doctor reports (ours and the guest's) and print the differences that matter.

Usage: python guest-doctor-compare.py OURS.md GUEST.md

Both files are made by guest-doctor.ps1. Only the standard library is used.
Exit code: 0 when the reports were read, 2 when a file cannot be read.
"""
import re
import sys

# Which section of the report belongs to which group of the output.
GROUPS = [
    ("System", ["System", "CPU and memory", "Disks"]),
    ("GPU and driver", ["GPU and driver"]),
    ("Settings", ["Windows settings", "Components", "Firewall"]),
    ("Mitigations and link tests", ["Mitigations", "Link tests"]),
    ("Title paths", ["Title paths"]),
    ("Tool versions", ["Tools"]),
]

# Checks that change from run to run or from machine to machine without meaning.
NOISE = re.compile(r"^(Uptime|Virtual memory|Parent \d+:.*|Drive [A-Z]:|Region and time zone|Decant build)$")
# Numbers inside these values change all the time (free space, dedicated memory).
FREE_SPACE = re.compile(r"[\d.,]+ (GB|MB) free of ")


def read_report(path):
    """Returns {section: {check: (status, value)}} from the tables of a report."""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().splitlines()
    except OSError as e:
        print("cannot read %s: %s" % (path, e), file=sys.stderr)
        sys.exit(2)
    out, section = {}, None
    for line in lines:
        if line.startswith("## "):
            section = line[3:].strip()
            out.setdefault(section, {})
        elif section and line.startswith("| ") and not line.startswith("| Status"):
            cells = [c.strip() for c in line.strip().strip("|").split(" | ")]
            if len(cells) >= 3:
                out[section][cells[1]] = (cells[0], cells[2])
    return out


def flat(report, sections):
    rows = {}
    for s in sections:
        for check, val in report.get(s, {}).items():
            if not NOISE.match(check):
                rows[(s, check)] = val
    return rows


def show(name):
    return name[1]


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    ours, theirs = read_report(sys.argv[1]), read_report(sys.argv[2])
    total = 0
    for group, sections in GROUPS:
        a, b = flat(ours, sections), flat(theirs, sections)
        lines = []
        for key in sorted(set(a) | set(b)):
            sa, sb = a.get(key), b.get(key)
            if sa == sb:
                continue
            if sa and sb and sa[1] == sb[1] and sa[0] == sb[0]:
                continue
            check = show(key)
            if sa is None:
                lines.append("  only on the guest: %s = %s [%s]" % (check, sb[1], sb[0]))
            elif sb is None:
                lines.append("  only on ours:      %s = %s [%s]" % (check, sa[1], sa[0]))
            else:
                if FREE_SPACE.search(sa[1]) and FREE_SPACE.search(sb[1]) and sa[0] == sb[0]:
                    continue
                lines.append("  %s\n      ours : [%s] %s\n      guest: [%s] %s" % (check, sa[0], sa[1], sb[0], sb[1]))
        print("== %s: %s" % (group, "no differences that matter" if not lines else "%d difference(s)" % len(lines)))
        for l in lines:
            print(l)
        total += len(lines)
        print()
    print(hint(ours, theirs))
    return 0


def hint(ours, theirs):
    """One paragraph: what could explain a failure at the start of a title."""
    notes = []
    gs = theirs.get("System", {})
    lang = gs.get("Language and format")
    olang = ours.get("System", {}).get("Language and format")
    if lang and olang and lang[1] != olang[1]:
        notes.append("the language or format differs, and our layer passes the user locale to the title unchanged")
    gpu = theirs.get("GPU and driver", {})
    ogpu = ours.get("GPU and driver", {})
    if any(gpu.get(k) != ogpu.get(k) for k in set(gpu) | set(ogpu) if k.startswith(("GPU", "DirectX"))):
        notes.append("the GPU, its driver or the DirectX level differs")
    comp = theirs.get("Components", {})
    ocomp = ours.get("Components", {})
    if any(comp.get(k) != ocomp.get(k) for k in comp if k.startswith(("Service", "System file xgameruntime", "Gaming Services"))):
        notes.append("Gaming Services, GameInput or xgameruntime.dll differ, and the PC runtime tables we forward reach the title from there")
    if any(k.startswith("Parent") and "ENFORCED" in v[1] for k, v in theirs.get("Mitigations", {}).items()) or        "ENFORCED" in theirs.get("Mitigations", {}).get("RedirectionTrust, this process", ("", ""))[1]:
        notes.append("RedirectionTrust is enforced above the doctor, which explains error 448 for junctions but not a failure at start up")
    text = ("Hint for a failure at the start of a title: our runs pass this point, so something on the other machine differs. Candidates: "
            "1) timing, 2) the locale and region that we pass through, 3) the PC runtime (Gaming Services, GameInput) and the GPU driver.")
    if notes:
        text += " This comparison shows: " + ". Also ".join(notes) + "."
    else:
        text += " This comparison shows no difference in those areas, so look at timing first."
    return text


if __name__ == "__main__":
    sys.exit(main())

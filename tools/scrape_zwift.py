"""Récupère les séances vélo de whatsonzwift.com (plans d'entraînement et
collections de séances) et écrit data/zwift_plans.json, au format `Workout`
de CyclAI (voir src/workout.rs).

    python tools/scrape_zwift.py [--cache DOSSIER]

Un plan ou une collection = une page (`/workouts/<slug>`) qui contient toutes
ses séances (les plans les rangent par semaine). Seuls les plans 100 % vélo de
la section « Zwift training plans » sont gardés (course à pied et multisport
ignorés) ; dans les « Zwift workout collections », on garde les séances vélo
de chaque collection. Les collections « legacy » ne sont pas reprises.
`--cache` mémorise les pages téléchargées (pratique pour relancer l'analyse).

Les intensités restent en % de FTP (cibles `ftp_pct_range` et `ftp_ramp`) :
c'est l'application qui les convertit en watts avec la FTP des réglages.
Le fichier produit est embarqué dans le binaire (src/zwift.rs) et importé au
démarrage dans la catégorie « Depuis Zwift ». Les jours de repos (sans
étape) sont ignorés.
"""
import argparse
import html
import json
import re
import sys
import urllib.request
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8")

SITE = "https://whatsonzwift.com"
OUT = Path(__file__).resolve().parent.parent / "data" / "zwift_plans.json"
UA = {"User-Agent": "Mozilla/5.0"}
CACHE = None
CAT_PLANS = "Depuis Zwift · Plans d'entraînement"
CAT_COLLECTIONS = "Depuis Zwift · Collections"


def get(url):
    cached = CACHE / re.sub(r"\W+", "_", url) if CACHE else None
    if cached and cached.exists():
        return cached.read_text(encoding="utf-8")
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60) as r:
        body = r.read().decode("utf-8")
    if cached:
        cached.write_text(body, encoding="utf-8")
    return body


def text_of(fragment):
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", "", fragment))).strip()


# --- une barre de séance -> étapes ------------------------------------------------

def duration(text):
    """'1hr 5min', '1min 50sec' -> '65min', '110s'."""
    m = re.search(r"(?:(\d+)\s*hr)?\s*(?:(\d+)\s*min)?\s*(?:(\d+)\s*sec)?", text.strip())
    total = int(m.group(1) or 0) * 3600 + int(m.group(2) or 0) * 60 + int(m.group(3) or 0)
    return f"{total // 60}min" if total % 60 == 0 else f"{total}s"


def num(x):
    """'75' -> 75, '72.5' -> 72.5."""
    f = round(float(x), 1)
    return int(f) if f.is_integer() else f


def pct(a, b=None):
    if b is None or b == a:
        return {"type": "ftp_pct_range", "low": a, "high": a}
    return {"type": "ftp_ramp", "from": a, "to": b}


SEG = re.compile(
    r"^(?P<dur>(?:\d+\s*hr)?\s*(?:\d+\s*min)?\s*(?:\d+\s*sec)?)\s*(?:@\s*)?"
    r"(?:(?P<rpm>\d+)\s*rpm,?\s*)?(?P<rest>.*)$"
)


def parse_segment(seg):
    """'4min @ 90rpm, from 60 to 75% FTP' -> one leaf step."""
    m = SEG.match(seg.strip())
    if not m or not m.group("dur").strip():
        raise ValueError(f"segment illisible : {seg!r}")
    rest, step = m.group("rest").strip(), {"duration": {"type": "time", "value": duration(m.group("dur"))}, "terrain": "plat"}
    notes = []
    if m.group("rpm"):
        notes.append(f"{m.group('rpm')} rpm")
    if rest.startswith("free ride"):
        # No target on the site: drawn at an easy 60 % so the block is visible.
        t = re.search(r"target\s*(\d+(?:\.\d+)?)\s*%", rest)
        step["target"] = pct(num(t.group(1)) if t else 60)
        notes.append("Free ride")
    elif rest.startswith("MAX"):
        step["target"] = pct(150)
        notes.append("Effort maximal")
    elif (r := re.match(r"from\s*(\d+(?:\.\d+)?)\s*to\s*(\d+(?:\.\d+)?)\s*%\s*FTP", rest)):
        step["target"] = pct(num(r.group(1)), num(r.group(2)))
    elif (r := re.match(r"(\d+(?:\.\d+)?)\s*%\s*FTP", rest)):
        step["target"] = pct(num(r.group(1)))
    elif (r := re.match(r"from\s*(\d+(?:\.\d+)?)\s*to\s*(\d+(?:\.\d+)?)\s*W", rest)):
        # Garmin / Zwift ranges have no direction: low end first.
        lo, hi = sorted((num(r.group(1)), num(r.group(2))))
        step["target"] = {"type": "power_range", "low": lo, "high": hi}
    elif (r := re.match(r"(\d+(?:\.\d+)?)\s*W", rest)):
        step["target"] = {"type": "power_range", "low": num(r.group(1)), "high": num(r.group(1))}
    else:
        raise ValueError(f"cible illisible : {seg!r}")
    if notes:
        step["note"] = " · ".join(notes)
    return step


def level(step):
    t = step.get("target") or {}
    if t.get("type") == "ftp_ramp":
        return max(t["from"], t["to"])
    if t.get("type") == "ftp_pct_range":
        return t["high"]
    return 100  # absolute watts: neutral


def parse_bar(raw):
    """A textbar -> leaf steps, or one repeat block for 'Nx ...' bars."""
    text = re.sub(r'<span data-value="([\d.]+)"[^>]*>[^<]*</span>', r"\1", raw)
    text = html.unescape(text).strip()
    count = 1
    m = re.match(r"(\d+)x\s*(.*)", text, re.S)
    if m:
        count, text = int(m.group(1)), m.group(2)
    leaves = [parse_segment(seg) for seg in re.split(r"<br\s*/?>", text)]
    if count == 1:
        return leaves
    for i, s in enumerate(leaves):
        s["type"] = "interval" if i == 0 or level(s) > 60 and len(leaves) > 2 else "recovery"
    return [{"type": "repeat", "count": count, "steps": leaves}]


def sig(step):
    return json.dumps({k: v for k, v in step.items() if k != "type"}, sort_keys=True)


def compress(steps):
    """Zwift lists every repetition of a block: fold identical consecutive
    blocks (1 to 6 steps long) back into `repeat` steps."""
    out, i = [], 0
    while i < len(steps):
        best = (0, 0, 0)  # covered steps, period, repeats
        for p in range(1, 7):
            block = [sig(s) for s in steps[i:i + p]]
            if len(block) < p:
                break
            k = 1
            while [sig(s) for s in steps[i + k * p:i + (k + 1) * p]] == block:
                k += 1
            if k >= (3 if p == 1 else 2) and p * k > best[0]:
                best = (p * k, p, k)
        if best[0]:
            _, p, k = best
            block = steps[i:i + p]
            for j, s in enumerate(block):
                s["type"] = "interval" if j == 0 or level(s) > 60 and p > 2 else "recovery"
            out.append({"type": "repeat", "count": k, "steps": block})
            i += p * k
        else:
            out.append(steps[i])
            i += 1
    return out


def classify(steps):
    """Type each loose step: warmup / cooldown ramps at the ends, recovery
    when easy, interval otherwise (repeat blocks are typed by `compress`)."""
    for n, s in enumerate(steps):
        if s.get("type") == "repeat":
            continue
        t = s.get("target")
        if t is None:
            s["type"] = "interval"
        elif n == 0 and t["type"] == "ftp_ramp" and t["to"] > t["from"]:
            s["type"] = "warmup"
        elif n == len(steps) - 1 and t["type"] == "ftp_ramp" and t["to"] < t["from"]:
            s["type"] = "cooldown"
        else:
            s["type"] = "recovery" if level(s) <= 60 else "interval"


def build_steps(bars):
    flat = [s for bar in bars for s in parse_bar(bar)]
    steps, run = [], []
    for s in flat:
        if s.get("type") == "repeat":  # explicit "Nx" bar
            steps += compress(run)
            run = []
            steps.append(s)
        else:
            run.append(s)
    steps += compress(run)
    classify(steps)
    return steps


# --- pages -------------------------------------------------------------------------

def french_group(label):
    """'Week 3' -> 'Semaine 3', 'Week 0 Prep' -> 'Semaine 0 Prep'."""
    return re.sub(r"^Week\b", "Semaine", label)


def week_prefix(label):
    m = re.match(r"Semaine (\d+)\b", label or "")
    return f"S{m.group(1)} · " if m else ""


def french_title(title):
    return re.sub(r"^Day (\d+)\s*[-–]\s*", r"Jour \1 — ", title)


def cycling_plan_slugs():
    """Slugs of the 'Zwift training plans' section whose page has no running workout."""
    index = get(f"{SITE}/workouts")
    section = re.search(r"Zwift training plans</h2>(.*?)<h2", index, re.S).group(1)
    slugs = list(dict.fromkeys(re.findall(r'href="https://whatsonzwift.com/workouts/([^"#?/]+)"', section)))
    keep = []
    for slug in slugs:
        page = get(f"{SITE}/workouts/{slug}")
        if "flaticon-run" in page:
            print(f"{slug}: plan course / multisport, ignoré")
        else:
            keep.append(slug)
    return keep


def collection_slugs():
    """Slugs of the 'Zwift workout collections' section of the index."""
    index = get(f"{SITE}/workouts")
    section = re.search(r'id="zwift-workout-collections-heading".*?id="zwift-plans-heading"', index, re.S).group(0)
    return list(dict.fromkeys(re.findall(r'href="https://whatsonzwift.com/workouts/([^"#?/]+)"', section)))


def scrape_plan(slug, category):
    page = get(f"{SITE}/workouts/{slug}")
    plan = text_of(re.search(r"<h1[^>]*>(.*?)</h1>", page, re.S).group(1))
    entries, seen = [], set()
    for art in re.split(r"(?=<article\b)", page)[1:]:
        art = art.split("</article>")[0]
        if f'<article id="{slug}"' in art:
            continue  # the plan's own header
        crumbs, lis = [], re.findall(r"<li>(.*?)</li>", art.split("</ol>")[0], re.S)
        if not lis or "flaticon-bike" not in lis[-1]:
            continue  # running workout
        for li in lis:
            href = re.search(r'href="([^"]*)"', li)
            crumbs.append((href.group(1) if href else "", text_of(li)))
        bars = re.findall(r'<div class="textbar"[^>]*>(.*?)</div>', art, re.S)
        if len(crumbs) < 3 or not bars:
            continue  # rest day / no structure
        href, title = crumbs[-1]
        wslug = href.split(f"/workouts/{slug}/")[-1].strip("/")
        if not wslug or wslug in seen:
            continue
        seen.add(wslug)
        subgroup = french_group(" · ".join(label for _, label in crumbs[2:-1])) or None
        desc = re.search(r"<p[^>]*>((?:(?!</p>).)*?)</p>\s*<ul", art, re.S)
        entries.append({
            "external_id": f"zwift:{slug}/{wslug}",
            "category": category,
            "plan_key": slug,
            "plan": plan,
            "subgroup": subgroup,
            "url": f"{SITE}/workouts/{slug}/{wslug}",
            "description": text_of(desc.group(1)) if desc else "",
            "workout": {"name": f"{plan} · {week_prefix(subgroup)}{french_title(title)}", "steps": build_steps(bars)},
        })
    return entries


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", type=Path)
    args = ap.parse_args()
    if args.cache:
        args.cache.mkdir(parents=True, exist_ok=True)
        CACHE = args.cache
    data = []
    for category, slugs in ((CAT_PLANS, cycling_plan_slugs()), (CAT_COLLECTIONS, collection_slugs())):
        for slug in slugs:
            entries = scrape_plan(slug, category)
            print(f"{slug}: {len(entries)} séances")
            data += entries
    OUT.parent.mkdir(exist_ok=True)
    OUT.write_text("[\n" + ",\n".join(json.dumps(e, ensure_ascii=False, separators=(",", ":")) for e in data) + "\n]\n",
                   encoding="utf-8")
    print(f"{len(data)} séances écrites dans {OUT}", file=sys.stderr)

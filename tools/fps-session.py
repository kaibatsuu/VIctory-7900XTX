# Frame-rate report for one play session, following docs/klbf/MEASUREMENT.md.
# Usage: python fps-session.py <log file> [--gpu "RX 9070 XT 16 GB"] [--cpu "..."] [--build "..."] [--out report.json]
# Reads the per-second "N fps, ... M draws/frame" lines; keeps samples with the world on screen (>= 1000 draws).
# Prints the three tables, a one-line summary to paste anywhere, and writes a JSON file others can compare.
# Duration comes from the log's own timestamps, not from what anyone types; the line ends with the log's fingerprint
# (SHA-256), so anyone given the log can rerun this tool and check the line was not edited.
import argparse, codecs, hashlib, io, json, re
ap = argparse.ArgumentParser()
ap.add_argument('log'); ap.add_argument('--gpu', default='?'); ap.add_argument('--cpu', default='?')
ap.add_argument('--build', default='?'); ap.add_argument('--out', default='fps-report.json')
a = ap.parse_args()
ansi = re.compile(r'\x1b\[[0-9;]*m')
from datetime import datetime
from statistics import median
entry = re.compile(r'^\s*\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}')
rate = re.compile(r'\b\d+(?:\.\d+)? fps,')
sample = re.compile(r'\b(\d+(?:\.\d+)?) fps, (\d+(?:\.\d+)?) ms/frame.*?(\d+) draws/frame')


def session_samples(lines):
    # PowerShell can wrap one tracing event across multiple physical lines.
    parts = []
    have_rate = False
    for line in lines:
        line = ansi.sub('', line).strip()
        starts_entry = entry.match(line)
        has_rate = rate.search(line) is not None
        if starts_entry or (have_rate and has_rate):
            parts = []
            have_rate = False
        if parts or has_rate or (starts_entry and 'victory_gpu::host:' in line):
            parts.append(line)
            have_rate |= has_rate
            if 'draws/frame' in line:
                yield ' '.join(parts)
                parts = []
                have_rate = False


def read_session(path):
    with open(path, 'rb') as raw:
        prefix = raw.peek(2)[:2]
        encoding = 'utf-16' if prefix in (codecs.BOM_UTF16_LE, codecs.BOM_UTF16_BE) else 'utf-8-sig'
        with io.TextIOWrapper(raw, encoding=encoding, errors='replace') as lines:
            yield from session_samples(lines)


v, t = [], []
bad = 0
for line in read_session(a.log):
    m = sample.search(line)
    if m and int(m.group(3)) >= 1000:
        fps, ms = float(m.group(1)), float(m.group(2))
        # Each line carries both the rate and the frame time: an edited rate no longer matches its frame time.
        if ms > 0 and abs(fps - 1000 / ms) > max(0.6, 0.08 * fps):
            bad += 1
        v.append(fps)
        ts = re.match(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)', line)
        if ts:
            d = datetime.fromisoformat(ts.group(1))
            if t and d < t[-1]:
                bad += 1  # time going backwards: lines moved or pasted
            t.append(d)
if not v:
    raise SystemExit('no world samples found')
s = sorted(v); n = len(s)
low = lambda p: round(sum(s[:max(1, n * p // 100)]) / max(1, n * p // 100), 1)
bands = [(18, 1e9, '18+'), (15, 18, '15-18'), (12, 15, '12-15'), (9, 12, '9-12'), (5, 9, '5-9'), (0, 5, '<5')]
spread = {name: round(100 * sum(lo <= x < hi for x in v) / n) for lo, hi, name in bands}
hasher = hashlib.sha256()
with open(a.log, 'rb') as raw:
    for chunk in iter(lambda: raw.read(1024 * 1024), b''):
        hasher.update(chunk)
digest = hasher.hexdigest()[:16]
r = {'method': 'KLBF Lab shared measurement (docs/klbf/MEASUREMENT.md)', 'build': a.build, 'gpu': a.gpu, 'cpu': a.cpu,
     'minutes': round((t[-1] - t[0]).total_seconds() / 60, 1) if len(t) > 1 else 0, 'average': round(sum(v) / n, 1), 'median': round(median(s), 2), 'low_5pct': low(5), 'low_1pct': low(1),
     'peak': s[-1], 'stability_index_pct': spread, 'samples': n,
     'from_utc': t[0].isoformat() if t else '', 'to_utc': t[-1].isoformat() if t else '', 'log_sha256_16': digest, 'consistency': 'ok' if bad == 0 else f'{bad} lines do not add up'}
print(f"Average performance index: average {r['average']} fps, median {r['median']}, 5 % low {r['low_5pct']}, 1 % low {r['low_1pct']} ({n} samples, {r['minutes']} min)")
print('Stability index: ' + ', '.join(f'{k} {p} %' for k, p in spread.items()))
share = (f"[KLBF benchmark] {a.build} | {a.gpu} | {a.cpu} | {r['minutes']} min real play | avg {r['average']} fps, "
         f"median {r['median']}, 1% low {r['low_1pct']}, peak {r['peak']} | time at 12+ fps: {round(100 * sum(fps >= 12 for fps in v) / n)} % "
         f"| {n} samples {r['from_utc'][11:16]}-{r['to_utc'][11:16]} UTC | log {digest} | check {r['consistency']}")
print('\nShare this line:\n' + share)
json.dump(r, open(a.out, 'w', encoding='utf8'), indent=2)
print(f'\nSaved {a.out}')

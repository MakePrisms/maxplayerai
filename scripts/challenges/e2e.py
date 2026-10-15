#!/usr/bin/env python3
"""Real local Buzz + actual CLI/MCP/sellers + community HTTP projection. No paid jobs."""
import argparse
import datetime as dt
import glob
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument('--skip-build', action='store_true', help='use binaries already built by run-e2e.sh')
parser.add_argument('--baseline', action='store_true', help='exercise existing free-job flow before challenge CLI lands')
ARGS = parser.parse_args()
# Turn termination into normal exception unwinding so recorded processes are cleaned up.
def interrupted(signum, frame):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, interrupted)
RUN = Path(tempfile.mkdtemp(prefix='ce-', dir=ROOT / '.scratch'))
os.chmod(RUN, 0o700)
# Keep buyer Unix socket below sockaddr_un's 108-byte path limit.
BIN = ROOT / 'target/challenges-e2e/debug/maxplayer'
BUZZ = ROOT / 'target/challenges-buzz/debug/buzz-relay'
PROCESSES = []
ENV = {k: v for k, v in os.environ.items() if k in ['PATH', 'HOME', 'USER', 'LANG', 'LD_LIBRARY_PATH', 'SSL_CERT_FILE', 'NIX_SSL_CERT_FILE']}
ENV.update({'RUST_LOG': 'info', 'TMPDIR': str(RUN)})

def executable(name, pattern):
    found = shutil.which(name) or next(iter(sorted(glob.glob(pattern), reverse=True)), None)
    if not found:
        raise RuntimeError(f'Missing {name}; install PostgreSQL/Redis (see README)')
    return str(found)

def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

def command(argv, env=None, timeout=100):
    p = subprocess.run(list(map(str, argv)), cwd=ROOT, env=env or ENV, text=True, capture_output=True, timeout=timeout)
    if p.returncode:
        raise RuntimeError(f'{Path(str(argv[0])).name} {argv[1:3]} exited {p.returncode}: {p.stderr[-4000:]} {p.stdout[-2000:]}')
    return p.stdout

def start(name, argv, env=None):
    log = open(RUN / (name + '.log'), 'w')
    p = subprocess.Popen(list(map(str, argv)), cwd=ROOT, env=env or ENV, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    log.close()
    PROCESSES.append((name, p))
    return p

def wait(label, fn, seconds=90):
    end = time.monotonic() + seconds
    last = None
    while time.monotonic() < end:
        for name, p in PROCESSES:
            if p.poll() is not None:
                raise RuntimeError(f'{name} exited {p.returncode}: {(RUN / (name + ".log")).read_text()[-3500:]}')
        try:
            result = fn()
            if result:
                print('PASS ' + label, flush=True)
                return result
        except (ConnectionError, OSError, ValueError) as e:
            last = str(e)
        time.sleep(.5)
    raise AssertionError(f'{label}: timeout; last={last}')

def check(label, value):
    if not value:
        raise AssertionError(label)
    print('PASS ' + label, flush=True)

def read_json(url):
    with urllib.request.urlopen(url, timeout=3) as r:
        return json.load(r)

def home(name, relay, buyer=None):
    path = RUN / name
    path.mkdir(mode=0o700)
    config = f'relay_url = "{relay}"\naccepted_mints = []\nallow_real_mints = false\nper_job_budget_sats = 0\n'
    config += '\n[review]\nseller_offer = false\nbuyer_delivery = false\n[review.reviewers]\n\n[platform_fee]\nauto_remit = false\n[seller_preflight]\nboot_push_preflight = false\n'
    if buyer:
        config += f'\n[seller]\nagent_command = ["node", {json.dumps(str(ROOT / "scripts/challenges/stub-acp-agent.mjs"))}]\nrate_sats = 0\ntakes_no_payment = true\ngit_remote = "http://127.0.0.1:1/unused.git"\naccept_offers_only_from = ["{buyer}"]\naccept_open_targeted = false\nclaim_open_pool = false\n'
    (path / 'config.toml').write_text(config)
    env = {**ENV, 'MAXPLAYER_HOME': str(path)}
    out = command([BIN, 'whoami'], env)
    return env, re.search(r'pubkey: ([0-9a-f]{64})', out)[1]

def mcp(env, tool, args):
    if tool == 'post_job':
        assert args['payment'] == 'none' and args['amount_sats'] == args['max_sats'] == 0
        assert args['visibility'] == 'public' and args['seller_pubkey'] == SELLER
    out = command(['node', ROOT / 'scripts/challenges/mcp-call.mjs', BIN, tool, json.dumps(args)], env)
    result = json.loads(out)
    (RUN / f'{tool}-latest.json').write_text(json.dumps(result, indent=2))
    return result

def iso(t):
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')

def cli(env, *args):
    return command([BIN, 'challenge', *args], env)

def plan(snapshot):
    path = RUN / 'snapshot.json'
    path.write_text(json.dumps(snapshot))
    return json.loads(command(['node', ROOT / 'scripts/challenges/coordinator.mjs', path, '--duration-seconds', '120']))

try:
    print(f'Artifacts (private, ignored): {RUN}', flush=True)
    check('integration binaries exist', BIN.is_file() and BUZZ.is_file())
    initdb = executable('initdb', '/nix/store/*-postgresql-17.*/bin/initdb')
    postgres = str(Path(initdb).with_name('postgres'))
    redis = executable('redis-server', '/nix/store/*-redis-*/bin/redis-server')
    pgport, redisport, relayport, appport = [port() for _ in range(4)]
    command([initdb, '-D', RUN / 'pg', '--auth=trust', '--no-locale', '--encoding=UTF8'])
    start('postgres', [postgres, '-D', RUN / 'pg', '-h', '127.0.0.1', '-p', pgport, '-c', 'unix_socket_directories='])
    start('redis', [redis, '--bind', '127.0.0.1', '--port', redisport, '--save', '', '--appendonly', 'no'])
    pgisready = str(Path(initdb).with_name('pg_isready'))
    wait('Postgres ready', lambda: subprocess.run([pgisready, '-h', '127.0.0.1', '-p', str(pgport)], stdout=subprocess.DEVNULL).returncode == 0)
    relay = f'ws://127.0.0.1:{relayport}'
    start('buzz', [BUZZ], {**ENV, 'RELAY_URL': relay, 'DATABASE_URL': f'postgresql://{os.environ["USER"]}@127.0.0.1:{pgport}/postgres', 'REDIS_URL': f'redis://127.0.0.1:{redisport}', 'BUZZ_BIND_ADDR': f'127.0.0.1:{relayport}', 'BUZZ_HEALTH_PORT': '0', 'BUZZ_METRICS_PORT': '0', 'BUZZ_RELAY_PRIVATE_KEY': secrets.token_hex(32), 'BUZZ_AUTO_MIGRATE': 'true', 'BUZZ_OPEN_READ': 'true', 'BUZZ_GIT_CONFORMANCE_PROBE': 'false', 'BUZZ_S3_ENDPOINT': 'http://127.0.0.1:1', 'BUZZ_GIT_REPO_PATH': str(RUN / 'git'), 'BUZZ_GIT_PACK_CACHE_PATH': str(RUN / 'packs')})
    wait('local Buzz ready', lambda: urllib.request.urlopen(f'http://127.0.0.1:{relayport}/health', timeout=2).status == 200)
    buyer_env, BUYER = home('b', relay)
    seller_env, SELLER = home('s', relay, BUYER)
    denied_env, DENIED = home('d', relay, BUYER)
    check('three distinct disposable identities', len({BUYER, SELLER, DENIED}) == 3)
    start('buyer', [BIN, 'buyer', 'serve'], buyer_env)
    for name, env in [('seller', seller_env), ('denied', denied_env)]:
        start(name, [BIN, 'seller', '--non-interactive', '--skip-doctor'], env)
    # Buyer foreground service owns its daemon lifecycle; no operator home is consulted.
    time.sleep(2)
    if not ARGS.baseline:
        start('app', ['node', ROOT / 'web/community/src/server.mjs'], {**ENV, 'RELAY_URL': relay, 'CHALLENGE_BUYER_PUBKEY': BUYER, 'APP_AUTH_KEY_FILE': str(RUN / 'app-key'), 'PORT': str(appport), 'CORS_ORIGIN': f'http://127.0.0.1:{appport}'})
        url = f'http://127.0.0.1:{appport}/api/challenges/current'
        snapshot = lambda: read_json(url)
        now = int(time.time())
        goal = RUN / 'goal.md'
        goal.write_text('Return the fixture answer 42. No paid work.\n')
        cli(buyer_env, 'publish', '--id', 'local-e2e', '--title', 'Free local challenge', '--goal-file', str(goal), '--join-opens', iso(now - 3600), '--starts', iso(now), '--ends', iso(now + 900), '--budget-sats', '0', '--status', 'in_progress')
        wait('challenge visible in app', lambda: snapshot()['challenge'])
        stream_frames = []
        def consume_stream():
            try:
                with urllib.request.urlopen(url + '/stream', timeout=120) as response:
                    for line in response:
                        if line.startswith(b'data: '):
                            stream_frames.append(json.loads(line[6:]))
            except (OSError, ValueError):
                pass  # Missing lifecycle frames fail the assertions below.
        threading.Thread(target=consume_stream, daemon=True).start()
        wait('SSE initial snapshot received', lambda: stream_frames and stream_frames[0]['challenge']['id'] == 'local-e2e')
        cli(seller_env, 'apply', '--buyer', BUYER, '--challenge', 'local-e2e', '--minutes', '60')
        cli(denied_env, 'apply', '--buyer', BUYER, '--challenge', 'local-e2e', '--minutes', '30')
        s = wait('both applications visible (60 and 30 minutes)', lambda: (s if sorted(x['minutesOffered'] for x in s['sellers']) == [30, 60] else None) if (s := snapshot()) else None)
        check('both initially applied', all(x['state'] == 'applied' for x in s['sellers']))
        cli(buyer_env, 'decide', '--challenge', 'local-e2e', '--seller', SELLER, '--accept')
        cli(buyer_env, 'decide', '--challenge', 'local-e2e', '--seller', DENIED, '--deny')
        s = wait('accepted/denied tally and authenticated history', lambda: (s if {x['state'] for x in s['sellers']} == {'accepted','denied'} and s['relay']['historyComplete'] else None) if (s := snapshot()) else None)
        p = plan(s)
        check('coordinator permits only accepted idle seller', p['postingAllowed'] and [x['seller_pubkey'] for x in p['sellers']] == [SELLER] and p['budgetHeadroomSats'] == 0)
        deadline = p['sellers'][0]['deadlineUnix']
    else:
        deadline = int(time.time()) + 120
    posted = mcp(buyer_env, 'post_job', {'task':'Return exactly the fixture answer 42.', 'output':'text/plain', 'payment':'none', 'amount_sats':0, 'max_sats':0, 'visibility':'public', 'seller_pubkey':SELLER, 'deadline_unix':deadline, 'request_id':'challenge-e2e-free'})
    job = posted['job_id']
    check('coordinator task posted through existing buyer MCP', bool(re.fullmatch('[0-9a-f]{64}', job)))
    if not ARGS.baseline:
        s = wait('claim and award yield running tally', lambda: (s if any(j['id'] == job and j['running'] and j['claim'] and j['award'] for j in s['jobs']) else None) if (s := snapshot()) else None)
        check('running seller not assignable', not plan(s)['postingAllowed'])
        wait('SSE running job received', lambda: any(any(j['id'] == job and j['running'] for j in f['jobs']) for f in stream_frames))
    def delivery():
        state = mcp(buyer_env, 'get_job', {'job_id':job})
        return state if state.get('results') else None
    delivered = wait('seller delivered result', delivery)
    check('only accepted seller claimed and delivered', delivered['awarded']['seller_pubkey'] == SELLER and delivered['claims'] and all(c['seller_pubkey'] == SELLER for c in delivered['claims']) and all(r['seller_pubkey'] == SELLER for r in delivered['results']))
    check('review fixture answer before collect', 'Challenge fixture result: 42.' in json.dumps(delivered['results']))
    collected = mcp(buyer_env, 'collect', {'job_id':job})
    check('collect has no payment and zero sats', collected['pay']['state'] == 'none' and collected['pay']['amount_sats'] == 0 and collected['pay']['attempt_id'] is None)
    check('collect materialized result', Path(collected['path']).exists())
    if not ARGS.baseline:
        s = wait('completed tally has positive minutes used', lambda: (s if any(x['pubkey'] == SELLER and x['state'] == 'done' and x['minutesUsed'] > 0 for x in s['sellers']) else None) if (s := snapshot()) else None)
        own = next(x for x in s['sellers'] if x['pubkey'] == SELLER)
        j = next(j for j in s['jobs'] if j['id'] == job)
        check('minutes exactly match award/result timestamps', abs(own['minutesUsed'] - max(0,min(j['end']-j['start'],j['deadline']-j['start']))/60) < 1e-9 and abs(own['minutesRemaining'] + own['minutesUsed'] - 60) < 1e-9)
        check('denied seller got no job/minutes', not any(j['seller'] == DENIED for j in s['jobs']) and next(x for x in s['sellers'] if x['pubkey'] == DENIED)['minutesUsed'] == 0)
        check('completed seller can receive next task', plan(s)['postingAllowed'])
        cli(seller_env, 'withdraw', '--buyer', BUYER, '--challenge', 'local-e2e')
        s = wait('withdraw visible', lambda: (s if any(x['pubkey'] == SELLER and x['state'] == 'withdrawn' for x in s['sellers']) else None) if (s := snapshot()) else None)
        p = plan(s)
        check('withdraw stops further assignment', not p['postingAllowed'] and p['sellers'] == [])
        wait('SSE withdrawal received', lambda: any(any(x['pubkey'] == SELLER and x['state'] == 'withdrawn' for x in f['sellers']) for f in stream_frames))
        check('withdraw preserves settled job/minutes', len(s['jobs']) == 1 and next(x for x in s['sellers'] if x['pubkey'] == SELLER)['minutesUsed'] == own['minutesUsed'])
        (RUN / 'final-snapshot.json').write_text(json.dumps(s, indent=2))
    print('PASS ALL ' + ('baseline free-job flow' if ARGS.baseline else 'community challenge E2E'), flush=True)
except Exception as e:
    print(f'FAIL {e}', flush=True)
    raise SystemExit(1)
finally:
    for name, p in reversed(PROCESSES):
        if p.poll() is None:
            os.killpg(p.pid, signal.SIGTERM)
    for name, p in reversed(PROCESSES):
        try:
            p.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()

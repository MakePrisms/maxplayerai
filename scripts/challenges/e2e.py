#!/usr/bin/env python3
"""Real local Buzz + actual CLI/MCP/sellers + shared direct-relay projection. No paid jobs."""
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
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser()
parser.add_argument('--skip-build', action='store_true', help='use binaries already built by run-e2e.sh')
parser.add_argument('--baseline', action='store_true', help='exercise existing free-job flow before challenge CLI lands')
parser.add_argument('--duplicate-history', action='store_true', help='replay historical claim frames to reproduce live/history overlap')
ARGS = parser.parse_args()
# Turn termination into normal exception unwinding so recorded processes are cleaned up.
def interrupted(signum, frame):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, interrupted)
RUN = Path(tempfile.mkdtemp(prefix='ce-', dir=ROOT / '.scratch'))
os.chmod(RUN, 0o700)
# Keep buyer Unix socket below sockaddr_un's 108-byte path limit.
BIN = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target/challenges-e2e')) / 'debug/maxplayer'
CHALLENGES = ROOT / 'crates/maxplayer-challenges/target/debug/maxplayer-challenges'
LOCAL_RELAY = ROOT / 'crates/maxplayer-challenges/target/debug/examples/local_relay'
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
    p = subprocess.Popen(list(map(str, argv)), cwd=ROOT, env=env or ENV, text=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = p.communicate(timeout=timeout)
    except BaseException:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.communicate()
        raise
    if p.returncode:
        raise RuntimeError(f'{Path(str(argv[0])).name} {argv[1:3]} exited {p.returncode}: {stderr[-4000:]} {stdout[-2000:]}')
    return stdout

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
        except RuntimeError as e:
            if "lifecycle wire history incomplete" not in str(e):
                raise
            last = str(e)
        except (ConnectionError, OSError, ValueError) as e:
            last = str(e)
        time.sleep(.5)
    raise AssertionError(f'{label}: timeout; last={last}')

def check(label, value):
    if not value:
        raise AssertionError(label)
    print('PASS ' + label, flush=True)

def home(name, relay, buyer=None):
    path = RUN / name
    path.mkdir(mode=0o700)
    config = f'relay_url = "{relay}"\naccepted_mints = []\nallow_real_mints = false\nper_job_budget_sats = 0\n'
    config += '\n[review]\nseller_offer = false\nbuyer_delivery = false\n[review.reviewers]\n\n[platform_fee]\nauto_remit = false\n[seller_preflight]\nboot_push_preflight = false\n'
    if buyer:
        config += f'\n[seller]\nagent_command = ["node", {json.dumps(str(ROOT / "scripts/challenges/stub-acp-agent.mjs"))}]\nrate_sats = 0\ntakes_no_payment = true\ngit_remote = "http://127.0.0.1:1/unused.git"\naccept_offers_only_from = ["{buyer}"]\naccept_open_targeted = false\nclaim_open_pool = false\n'
    (path / 'config.toml').write_text(config)
    env = {**ENV, 'MAXPLAYER_HOME': str(path), 'E2E_STUB_RELEASE': str(RUN / 'release'), 'E2E_STUB_DELAY_FILE': str(RUN / 'delay-ms')}
    out = command([BIN, 'whoami'], env)
    return env, re.search(r'pubkey: ([0-9a-f]{64})', out)[1]

def mcp(env, tool, args):
    if tool == 'post_job':
        assert args['payment'] == 'none' and args['amount_sats'] == args['max_sats'] == 0
        assert args['visibility'] == 'public' and args['seller_pubkey'] == SELLER
    for attempt in range(3):
        try:
            out = command(['node', ROOT / 'scripts/challenges/mcp-call.mjs', BIN, tool, json.dumps(args)], env)
            break
        except RuntimeError as error:
            # These existing operations are idempotent. Never retry arbitrary errors or
            # change a claim/request in response to a transient incomplete read.
            if tool not in ('get_job', 'collect') or 'lifecycle wire history incomplete' not in str(error) or attempt == 2:
                raise
            print(f'RETRY {tool}: incomplete relay history ({attempt + 1}/3)', flush=True)
            with open(RUN / (tool + '-retries.log'), 'a') as log:
                log.write(str(error) + '\n')
            time.sleep(1)
    result = json.loads(out)
    (RUN / f'{tool}-latest.json').write_text(json.dumps(result, indent=2))
    return result

def iso(t):
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')

def cli(env, *args):
    return command([CHALLENGES, *args, '--relay', CHALLENGE_RELAY], env)

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
    pgport, redisport, relayport = [port() for _ in range(3)]
    command([initdb, '-D', RUN / 'pg', '--auth=trust', '--no-locale', '--encoding=UTF8'])
    start('postgres', [postgres, '-D', RUN / 'pg', '-h', '127.0.0.1', '-p', pgport, '-c', 'unix_socket_directories='])
    start('redis', [redis, '--bind', '127.0.0.1', '--port', redisport, '--save', '', '--appendonly', 'no'])
    pgisready = str(Path(initdb).with_name('pg_isready'))
    wait('Postgres ready', lambda: subprocess.run([pgisready, '-h', '127.0.0.1', '-p', str(pgport)], stdout=subprocess.DEVNULL).returncode == 0)
    frontport = port() if ARGS.duplicate_history else relayport
    relay = f'ws://127.0.0.1:{frontport}'
    if ARGS.duplicate_history:
        start('duplicate-history', ['python3', ROOT / 'scripts/challenges/duplicate-history.py', frontport, relayport])
    start('buzz', [BUZZ], {**ENV, 'RELAY_URL': relay, 'DATABASE_URL': f'postgresql://{os.environ["USER"]}@127.0.0.1:{pgport}/postgres', 'REDIS_URL': f'redis://127.0.0.1:{redisport}', 'BUZZ_BIND_ADDR': f'127.0.0.1:{relayport}', 'BUZZ_HEALTH_PORT': '0', 'BUZZ_METRICS_PORT': '0', 'BUZZ_RELAY_PRIVATE_KEY': secrets.token_hex(32), 'BUZZ_AUTO_MIGRATE': 'true', 'BUZZ_OPEN_READ': 'true', 'BUZZ_RATE_LIMIT_HUMAN_WS_EVENTS_PER_SEC': '1000', 'BUZZ_RATE_LIMIT_HUMAN_MESSAGES_PER_MIN': '10000', 'BUZZ_GIT_CONFORMANCE_PROBE': 'false', 'BUZZ_S3_ENDPOINT': 'http://127.0.0.1:1', 'BUZZ_GIT_REPO_PATH': str(RUN / 'git'), 'BUZZ_GIT_PACK_CACHE_PATH': str(RUN / 'packs')})
    wait('local Buzz ready', lambda: urllib.request.urlopen(f'http://127.0.0.1:{relayport}/health', timeout=2).status == 200)
    challenge_port = port()
    CHALLENGE_RELAY = f'ws://127.0.0.1:{challenge_port}'
    start('challenge-relay', [LOCAL_RELAY, challenge_port])
    wait('local generic challenge relay ready', lambda: socket.create_connection(('127.0.0.1', challenge_port), timeout=1).close() is None)
    challenge_relays = os.environ.get('E2E_CHALLENGE_RELAYS', CHALLENGE_RELAY)
    ENV['SWARM_CHALLENGE_RELAYS'] = challenge_relays
    buyer_env, BUYER = home('b', relay)
    seller_env, SELLER = home('s', relay, BUYER)
    denied_env, DENIED = home('d', relay, BUYER)
    check('three distinct disposable identities', len({BUYER, SELLER, DENIED}) == 3)
    if not ARGS.baseline:
        # Build a static page pinned to this disposable relay and buyer, never production.
        command(['npm', 'run', 'build', '--prefix', ROOT / 'web/app'], {**ENV, 'RELAY_URL': relay, 'CHALLENGE_BUYER_PUBKEY': BUYER, 'CHALLENGE_RELAYS': challenge_relays})
        siteport = port()
        start('site', ['python3', '-m', 'http.server', siteport, '--bind', '127.0.0.1', '--directory', ROOT / 'web/app/dist'])
        print(f'Browser fixture: http://127.0.0.1:{siteport}/challenges.html', flush=True)
        (RUN / 'browser.json').write_text(json.dumps({'url': f'http://127.0.0.1:{siteport}/challenges.html', 'relay': relay, 'buyer': BUYER}))
    start('buyer', [BIN, 'buyer', 'serve'], buyer_env)
    for name, env in [('seller', seller_env), ('denied', denied_env)]:
        start(name, [BIN, 'seller', '--non-interactive', '--skip-doctor'], env)
    # Buyer foreground service owns its daemon lifecycle; no operator home is consulted.
    time.sleep(2)
    if not ARGS.baseline:
        projection = RUN / 'relay-snapshot.json'
        start('projection', ['node', ROOT / 'scripts/challenges/relay-snapshot.mjs', relay, BUYER, projection, challenge_relays])
        snapshot = lambda: json.loads(projection.read_text())
        now = int(time.time())
        starts = now + 45
        goal = RUN / 'goal.md'
        goal.write_text('Return the fixture answer 42. No paid work.\n')
        cli(buyer_env, 'publish', '--id', 'local-e2e', '--title', 'Free local challenge', '--goal-file', str(goal), '--starts', iso(starts), '--ends', iso(now + 900), '--budget-sats', '0', '--status', 'in_progress')
        c = wait('challenge visible from relay', lambda: snapshot()['challenge'])
        check('challenge exposes only supported snapshot fields', set(c) == {'id','eventId','buyer','buyerNpub','title','goal','starts','ends','status','result'} and c['starts'] == starts)
        def frames():
            return [json.loads(line) for line in (RUN / 'projection.log').read_text().splitlines() if line.startswith('{')]
        wait('relay initial snapshot received', lambda: any(f['challenge'] and f['challenge']['id'] == 'local-e2e' for f in frames()))
        cli(seller_env, 'join', '--buyer', BUYER, '--minutes', '60')
        cli(denied_env, 'join', '--buyer', BUYER, '--minutes', '30')
        s = wait('both memberships visible (60 and 30 minutes)', lambda: (s if sorted(x['minutesOffered'] for x in s['sellers']) == [30, 60] else None) if (s := snapshot()) else None)
        check('both initially joined', all(x['state'] == 'joined' for x in s['sellers']))
        cli(buyer_env, 'decide', '--seller', SELLER, '--accept')
        cli(buyer_env, 'decide', '--seller', DENIED, '--deny')
        s = wait('accepted/denied tally and authenticated history', lambda: (s if {x['state'] for x in s['sellers']} == {'accepted','denied'} and s['relay']['historyComplete'] else None) if (s := snapshot()) else None)
        rows = json.loads(cli(buyer_env, 'members', '--challenge', 'local-e2e', '--json'))['members']
        check('Rust CLI and shared model agree on eligibility', len(rows) == 2 and [r['membership']['seller'] for r in rows if r['eligible']] == [SELLER])
        if int(time.time()) < starts:
            check('the buyer blocks before work window', 'outside_work_window' in plan(snapshot())['reasons'])
        wait('challenge work window open', lambda: int(time.time()) >= starts)
        s = wait('fresh complete relay snapshot before assignment', lambda: (s if s['relay']['historyComplete'] and s['relay']['authenticated'] and int(time.time()) - s['asOf'] <= 2 else None) if (s := snapshot()) else None)
        p = plan(s)
        check('the buyer permits only accepted idle seller', p['postingAllowed'] and [x['seller_pubkey'] for x in p['sellers']] == [SELLER] and p['budgetHeadroomSats'] == 0)
        deadline = p['sellers'][0]['deadlineUnix']
    else:
        deadline = int(time.time()) + 120
    posted = mcp(buyer_env, 'post_job', {'task':f'[challenge:{BUYER}:local-e2e]\nReturn exactly the fixture answer 42.', 'output':'text/plain', 'payment':'none', 'amount_sats':0, 'max_sats':0, 'visibility':'public', 'seller_pubkey':SELLER, 'deadline_unix':deadline, 'request_id':'challenge-e2e-free'})
    job = posted['job_id']
    check('the buyer posts through existing MCP', bool(re.fullmatch('[0-9a-f]{64}', job)))
    def claimed():
        state = mcp(buyer_env, 'get_job', {'job_id': job})
        return state if any(c['seller_pubkey'] == SELLER and c['live'] for c in state['claims']) else None
    state = wait('accepted seller live claim observed', claimed)
    def awarded():
        state = mcp(buyer_env, 'get_job', {'job_id': job})
        return state if state.get('awarded') else None
    state = wait('buyer auto-award committed without manual override', awarded, seconds=60)
    check('buyer exposes confirmed award read and reservation/claim price hints', state.get('award_read_confirmed') is True and state.get('reservation_state') in ['none', 'reserved', 'spent', 'released'] and all(p['amount_sats'] is None for p in state['claim_prices']))
    check('auto-award selected accepted seller', state['awarded']['seller_pubkey'] == SELLER)
    if not ARGS.baseline:
        s = wait('claim and award yield running tally', lambda: (s if any(j['id'] == job and j['running'] and j['claim'] and j['award'] for j in s['jobs']) else None) if (s := snapshot()) else None)
        check('running seller not assignable', not plan(s)['postingAllowed'])
        wait('relay streaming running job received', lambda: any(any(j['id'] == job and j['running'] for j in f['jobs']) for f in frames()))
    (RUN / 'release').write_text('deliver now\n')
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
        s = wait('completed tally has positive minutes used', lambda: (s if s['relay']['historyComplete'] and s['relay']['authenticated'] and not s['relay']['error'] and 0 <= int(time.time()) - s['asOf'] <= 2 and any(x['pubkey'] == SELLER and x['state'] == 'done' and x['minutesUsed'] > 0 for x in s['sellers']) and any(j['id'] == job and j['accepted'] for j in s['jobs']) else None) if (s := snapshot()) else None)
        own = next(x for x in s['sellers'] if x['pubkey'] == SELLER)
        j = next(j for j in s['jobs'] if j['id'] == job)
        check('minutes exactly match award/buyer-accept timestamps', abs(own['minutesUsed'] - max(0,min(j['end']-j['start'],j['deadline']-j['start']))/60) < 1e-9 and abs(own['minutesRemaining'] + own['minutesUsed'] - 60) < 1e-9)
        check('denied seller got no job/minutes', not any(j['seller'] == DENIED for j in s['jobs']) and next(x for x in s['sellers'] if x['pubkey'] == DENIED)['minutesUsed'] == 0)
        check('completed seller can receive next task', plan(s)['postingAllowed'])
        # A separately published second day reuses the very same membership and decision.
        def publish_day(day, begin, end):
            time.sleep(1.1)  # Distinct coordinates select current by publication timestamp.
            cli(buyer_env, 'publish', '--id', day, '--title', day, '--goal-file', str(goal),
                '--starts', iso(begin), '--ends', iso(end), '--budget-sats', '0', '--status', 'in_progress')
            return wait(day + ' visible', lambda: (s if s['challenge']['id'] == day else None) if (s := snapshot()) else None)
        day2 = publish_day('day-2', starts + 86400, starts + 90000)
        day2own = next(x for x in day2['sellers'] if x['pubkey'] == SELLER)
        check('join once eligible on day 1 and day 2 without rejoining or reaccepting', own['eligible'] and day2own['eligible'] and day2own['since'] == own['since'])
        check('minutes reset per day while prior jobs remain in history', day2own['minutesUsed'] == 0 and day2own['minutesRemaining'] == 60 and any(j['id'] == job for j in day2own['pastJobs']))
        rows = json.loads(cli(seller_env, 'status', '--buyer', BUYER, '--challenge', 'day-2', '--json'))['members']
        check('Rust confirms persistent day 2 eligibility', rows[0]['eligible'])
        publish_day('local-e2e', starts, now + 900)
        cli(seller_env, 'leave', '--buyer', BUYER)
        s = wait('leave visible', lambda: (s if any(x['pubkey'] == SELLER and x['state'] == 'left' for x in s['sellers']) else None) if (s := snapshot()) else None)
        p = plan(s)
        check('leave stops further assignment', not p['postingAllowed'] and p['sellers'] == [])
        wait('relay streaming leave received', lambda: any(any(x['pubkey'] == SELLER and x['state'] == 'left' for x in f['sellers']) for f in frames()))
        check('leave preserves settled job/minutes', len(s['jobs']) == 1 and next(x for x in s['sellers'] if x['pubkey'] == SELLER)['minutesUsed'] == own['minutesUsed'])
        rows = json.loads(cli(seller_env, 'status', '--buyer', BUYER, '--challenge', 'local-e2e', '--json'))['members']
        check('Rust CLI confirms left ineligible seller', len(rows) == 1 and not rows[0]['eligible'] and rows[0]['membership']['status'] == 'left')
        # Join just after work starts: even within 900 seconds there is no grace period.
        late_start = int(time.time()) - 1
        publish_day('late-day', late_start, late_start + 3600)
        cli(denied_env, 'leave', '--buyer', BUYER)
        cli(denied_env, 'join', '--buyer', BUYER, '--minutes', '30')
        cli(buyer_env, 'decide', '--seller', DENIED, '--accept')
        late = wait('late join visible and accepted', lambda: (s if any(x['pubkey'] == DENIED and x['membershipStatus'] == 'joined' and x['decision'] == 'accepted' and x['since'] > late_start for x in s['sellers']) else None) if (s := snapshot()) else None)
        lateown = next(x for x in late['sellers'] if x['pubkey'] == DENIED)
        check('join after starts cannot receive same-day assignments', not lateown['eligible'] and not plan(late)['postingAllowed'])
        rows = json.loads(cli(denied_env, 'status', '--buyer', BUYER, '--challenge', 'late-day', '--json'))['members']
        check('Rust confirms late join ineligible that day', not rows[0]['eligible'])
        nextday = publish_day('late-next-day', late_start + 86400, late_start + 90000)
        nextown = next(x for x in nextday['sellers'] if x['pubkey'] == DENIED)
        check('late join eligible next day without rejoining', nextown['eligible'] and nextown['since'] == lateown['since'])
        rows = json.loads(cli(denied_env, 'status', '--buyer', BUYER, '--challenge', 'late-next-day', '--json'))['members']
        check('Rust confirms late join eligible next day', rows[0]['eligible'])
        cli(denied_env, 'join', '--buyer', BUYER, '--minutes', '45')
        rows = json.loads(cli(denied_env, 'status', '--buyer', BUYER, '--challenge', 'late-next-day', '--json'))['members']
        check('changing daily minutes preserves membership since', rows[0]['membership']['since'] == lateown['since'] and rows[0]['membership']['minutes'] == 45)
        # One runner invocation per day: no harness post/award/collect calls in these scenarios.
        cli(seller_env, 'join', '--buyer', BUYER, '--minutes', '1')
        cli(denied_env, 'join', '--buyer', BUYER, '--minutes', '1')
        cli(buyer_env, 'decide', '--seller', SELLER, '--accept')
        cli(buyer_env, 'decide', '--seller', DENIED, '--accept')
        # Capability tokens are startup-probed, not configurable. Deterministic Rust-only
        # routing and unequal durations are asserted in swarm.test.mjs.
        def swarm_day(day, solve_after, duration):
            begin = int(time.time()) + 3
            publish_day(day, begin, begin + 600)
            runner = start(day + '-runner', ['node', ROOT / 'scripts/challenges/swarm.mjs',
                '--binary', BIN, '--relay-url', relay,
                '--planner', 'node scripts/challenges/planner-free-e2e.mjs',
                '--duration-seconds', str(duration), '--poll-seconds', '1'],
                {**buyer_env, 'SWARM_CHALLENGES_BINARY': str(CHALLENGES), 'SWARM_CHALLENGE_RELAYS': CHALLENGE_RELAY, 'SWARM_STUB_SOLVE_AFTER': str(solve_after), 'SWARM_MIN_DURATION': str(duration)})
            final = wait(day + ' status completed', lambda: (s if s['challenge']['id'] == day and s['challenge']['status'] == 'completed' else None) if (s := snapshot()) else None, seconds=240)
            def settled():
                log = Path(buyer_env['MAXPLAYER_HOME']) / 'swarm/events.jsonl'
                state = json.loads(log.read_text().strip().splitlines()[-1])
                jobs = [j for j in state['jobs'] if j['challenge']['id'] == day]
                return jobs if jobs and all(j.get('done') and j.get('outcome') == 'collected' for j in jobs) else None
            jobs = wait(day + ' all results collected', settled)
            check(day + ' both accepted sellers receive successive automatic jobs', all(sum(j['seller'] == seller for j in jobs) >= 2 for seller in [SELLER, DENIED]))
            check(day + ' zero payment', all(j['pay']['state'] == 'none' and j['pay']['amount_sats'] == 0 for j in jobs))
            count = len(jobs)
            time.sleep(3)
            check(day + ' never posts after stopping', len(settled()) == count)
            os.killpg(runner.pid, signal.SIGTERM)
            runner.wait(timeout=15)
            PROCESSES.remove((day + '-runner', runner))
            return final
        solved = swarm_day('swarm-solved', 4, 30)
        check('swarm solved: completed + result visible', solved['challenge']['result'] == 'Challenge fixture result: 42.')
        (RUN / 'delay-ms').write_text('10000')
        exhausted = swarm_day('swarm-exhausted', 100000, 40)
        check('swarm exhausted: completed + unsolved update visible', not exhausted['challenge']['result'] and any('Unsolved: all eligible sellers exhausted' in u for u in exhausted['updates']))
        (RUN / 'final-snapshot.json').write_text(json.dumps(s, indent=2))
    print('NOT EXERCISED paid settlement: this harness has no test mint/funded wallet; priced posting is unit-tested', flush=True)
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

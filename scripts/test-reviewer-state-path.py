#!/usr/bin/env python3
"""Exercise an evaluated reviewer launcher (with a no-op maxplayer package).

Usage: python3 scripts/test-reviewer-state-path.py /nix/store/...-maxplayer-reviewer-start
Only the state/runtime directory prefixes are remapped into a temporary fixture.
No real credentials, network, or provider calls are used.
"""
import ctypes
import ctypes.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

sql = ctypes.CDLL(ctypes.util.find_library("sqlite3"))
sql.sqlite3_open_v2.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p), ctypes.c_int, ctypes.c_char_p]
sql.sqlite3_close.argtypes = [ctypes.c_void_p]


def open_nofollow(path):
    db = ctypes.c_void_p()
    rc = sql.sqlite3_open_v2(os.fsencode(path), ctypes.byref(db), 2 | 4 | 0x01000000, None)
    sql.sqlite3_close(db)
    return rc


with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    real = root / "private" / "state"
    real.mkdir(parents=True)
    state = root / "state"
    state.symlink_to(real, target_is_directory=True)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    credentials = root / "credentials"
    credentials.mkdir()
    for name in ("signer", "typesafe"):
        (credentials / name).write_text("dummy-test-credential")
        (credentials / name).chmod(0o440)
    database = real / "reviews.sqlite"
    with sqlite3.connect(database) as db:
        db.execute("CREATE TABLE sentinel (value TEXT)")
        db.execute("INSERT INTO sentinel VALUES ('preserved')")
    source = Path(sys.argv[1]).read_text()
    source = source.replace("/var/lib/maxplayer-reviewer", str(state))
    source = source.replace("/run/maxplayer-reviewer", str(runtime))
    launcher = root / "start"
    launcher.write_text(source)
    env = dict(os.environ, CREDENTIALS_DIRECTORY=str(credentials))
    subprocess.run(["bash", "-n", launcher], check=True)
    for _ in range(2):
        subprocess.run(["bash", launcher], env=env, check=True)
        config = json.loads((runtime / "reviewer.json").read_text())
        assert config["database"] == str(database)
        assert open_nofollow(config["database"]) == 0
        for name in ("signer", "typesafe"):
            assert (runtime / name).stat().st_mode & 0o777 == 0o600
            assert (credentials / name).stat().st_mode & 0o777 == 0o440
            assert (runtime / name).read_bytes() == (credentials / name).read_bytes()
    with sqlite3.connect(database) as db:
        assert db.execute("SELECT value FROM sentinel").fetchone() == ("preserved",)
    assert open_nofollow(state / "reviews.sqlite") != 0, "old symlink path must reproduce failure"
    database.rename(real / "saved.sqlite")
    database.symlink_to(real / "saved.sqlite")
    subprocess.run(["bash", launcher], env=env, check=True)
    config = json.loads((runtime / "reviewer.json").read_text())
    assert open_nofollow(config["database"]) != 0, "database symlink must remain rejected"
    state.unlink()
    result = subprocess.run(["bash", launcher], env=env, capture_output=True)
    assert result.returncode != 0, "missing state directory must fail closed"
print("PASS: repeated startup, existing DB preservation, old-path negative control, DB symlink rejection, missing-directory failure, credential permissions")

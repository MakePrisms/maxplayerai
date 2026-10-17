#!/usr/bin/env python3
"""Loopback-only fault fixture: a byte-for-byte TCP proxy in front of a relay that opens one
coverage gap on demand. When TRIGGER appears it drops every connection, refuses new ones for
GAP seconds, then removes TRIGGER and resumes, so the runner's challenge reader reports
relay_not_ready deterministically between planning and posting.
"""
import asyncio
import os
import sys
import time

LISTEN, UPSTREAM, TRIGGER, GAP = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], float(sys.argv[4])
writers = set()
gap_until = 0.0

async def pipe(reader, writer):
    try:
        while chunk := await reader.read(65536):
            writer.write(chunk)
            await writer.drain()
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        writer.close()

async def connection(reader, writer):
    if time.monotonic() < gap_until:
        writer.close()
        return
    try:
        up_reader, up_writer = await asyncio.open_connection('127.0.0.1', UPSTREAM)
    except OSError:
        writer.close()
        return
    writers.update([writer, up_writer])
    try:
        await asyncio.gather(pipe(reader, up_writer), pipe(up_reader, writer))
    finally:
        writers.difference_update([writer, up_writer])

async def watch():
    global gap_until
    while True:
        await asyncio.sleep(0.1)
        if os.path.exists(TRIGGER) and time.monotonic() >= gap_until:
            gap_until = time.monotonic() + GAP
            for w in list(writers):
                w.close()
            print(f'gap opened for {GAP}s', flush=True)
            await asyncio.sleep(GAP)
            os.remove(TRIGGER)
            print('gap closed', flush=True)

async def main():
    server = await asyncio.start_server(connection, '127.0.0.1', LISTEN)
    async with server:
        await asyncio.gather(server.serve_forever(), watch())

asyncio.run(main())

#!/usr/bin/env python3
"""Loopback-only fault fixture: replay claim EVENT frames before their EOSE.

Buzz registers live fan-out before querying history. A concurrent publication can
therefore arrive both live and historically before EOSE. Reproduce that ordering
without relying on scheduler timing; preserve NIP-42 and all signed bytes.
"""
import asyncio
import json
import struct
import sys

async def connection(reader, writer):
    upstream, output = await asyncio.open_connection('127.0.0.1', int(sys.argv[2]))
    async def requests():
        while chunk := await reader.read(65536):
            output.write(chunk)
            await output.drain()
    async def responses():
        writer.write(await upstream.readuntil(b'\r\n\r\n'))
        await writer.drain()
        ended = set()
        while True:
            header = await upstream.readexactly(2)
            size = header[1] & 127
            if size in (126, 127):
                extension = await upstream.readexactly(2 if size == 126 else 8)
                size = struct.unpack('!H' if len(extension) == 2 else '!Q', extension)[0]
                header += extension
            # Server frames are unmasked; Buzz sends complete text frames.
            if header[1] & 128 or size > 16 * 1024 * 1024:
                raise ValueError('unexpected fixture frame')
            payload = await upstream.readexactly(size)
            frame = header + payload
            writer.write(frame)
            if header[0] == 0x81:
                message = json.loads(payload)
                if message[0] == 'EOSE':
                    ended.add(message[1])
                elif message[0] == 'EVENT' and message[1] not in ended and message[2]['kind'] == 3402:
                    writer.write(frame)
                    print('duplicated historical claim', flush=True)
            await writer.drain()
    tasks = [asyncio.create_task(requests()), asyncio.create_task(responses())]
    try:
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        writer.close()
        output.close()

async def main():
    server = await asyncio.start_server(connection, '127.0.0.1', int(sys.argv[1]))
    async with server:
        await server.serve_forever()

asyncio.run(main())

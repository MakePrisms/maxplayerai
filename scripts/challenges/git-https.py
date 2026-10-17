#!/usr/bin/env python3
"""Loopback-only HTTPS smart-git server for the swarm E2E file-delivery case.

Serves bare repositories under ROOT through `git http-backend` (anonymous read and push), with a
leaf certificate signed by a disposable local CA. Sellers push deliveries here and the buyer's
collect fetches them, exactly as they would from any credential-free https remote.
Usage: git-https.py PORT ROOT CERT KEY
"""
import os
import ssl
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, ROOT, CERT, KEY = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
BACKEND = os.path.join(subprocess.check_output(['git', '--exec-path'], text=True).strip(), 'git-http-backend')


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def body(self):
        if self.headers.get('Transfer-Encoding', '').lower() == 'chunked':
            data = b''
            while True:
                size = int(self.rfile.readline().split(b';')[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b'\r\n', b'\n', b''):
                        pass
                    return data
                data += self.rfile.read(size)
                self.rfile.readline()
        return self.rfile.read(int(self.headers.get('Content-Length') or 0))

    def run(self):
        path, _, query = self.path.partition('?')
        data = self.body() if self.command == 'POST' else b''
        env = {'PATH': os.environ.get('PATH', ''), 'GIT_PROJECT_ROOT': ROOT, 'GIT_HTTP_EXPORT_ALL': '1',
               'REQUEST_METHOD': self.command, 'PATH_INFO': path, 'QUERY_STRING': query,
               'CONTENT_TYPE': self.headers.get('Content-Type', ''), 'CONTENT_LENGTH': str(len(data)),
               'REMOTE_USER': 'e2e', 'REMOTE_ADDR': '127.0.0.1', 'GIT_HTTP_MAX_REQUEST_BUFFER': '100M'}
        if self.headers.get('Content-Encoding'):
            env['HTTP_CONTENT_ENCODING'] = self.headers['Content-Encoding']
        if self.headers.get('Git-Protocol'):
            env['GIT_PROTOCOL'] = self.headers['Git-Protocol']
        out = subprocess.run([BACKEND], input=data, env=env, capture_output=True).stdout
        head, _, payload = out.partition(b'\r\n\r\n')
        if not _:
            head, _, payload = out.partition(b'\n\n')
        status, headers = 200, []
        for line in head.decode('latin-1').splitlines():
            name, _, value = line.partition(':')
            if name.lower() == 'status':
                status = int(value.strip().split()[0])
            elif name:
                headers.append((name, value.strip()))
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        print(f'{self.command} {path} {status}', flush=True)

    do_GET = run
    do_POST = run

    def log_message(self, *args):
        pass


server = ThreadingHTTPServer(('127.0.0.1', PORT), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(CERT, KEY)
server.socket = context.wrap_socket(server.socket, server_side=True)
print(f'git https ready on {PORT}', flush=True)
server.serve_forever()

#!/usr/bin/env python3
"""极简 HTTP 代理：支持 CONNECT 隧道、http:// 绝对 URI、https:// 绝对 URI（自身做 TLS 中转）。
用途：给 reMarkable 2 通过 SSH 反向隧道借 Mac 的外网（BusyBox wget 不会用 CONNECT）。
"""
import socket, socketserver, select, ssl, sys

BUFSIZE = 65536

def pipe(a, b):
    try:
        while True:
            r, _, _ = select.select([a, b], [], [], 60)
            if not r:
                return
            for s in r:
                data = s.recv(BUFSIZE)
                if not data:
                    return
                (b if s is a else a).sendall(data)
    except Exception:
        pass

class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        line = self.rfile.readline(65536).decode('latin1').strip()
        if not line:
            return
        parts = line.split()
        if len(parts) < 2:
            return
        method, target = parts[0], parts[1]
        sys.stderr.write(f'REQ {method} {target[:120]}\n'); sys.stderr.flush()
        headers = {}
        while True:
            h = self.rfile.readline(65536).decode('latin1')
            if h in ('\r\n', '\n', ''):
                break
            if ':' in h:
                k, v = h.split(':', 1)
                headers[k.strip()] = v.strip()

        if method == 'CONNECT':
            host, _, port = target.partition(':')
            try:
                upstream = socket.create_connection((host, int(port or 443)), timeout=15)
            except Exception as e:
                sys.stderr.write(f'CONNECT FAIL {target} {e!r}\n'); sys.stderr.flush()
                self.wfile.write(b'HTTP/1.1 502 Bad Gateway\r\n\r\n')
                return
            self.wfile.write(b'HTTP/1.1 200 Connection Established\r\n\r\n')
            self.wfile.flush()
            pipe(self.connection, upstream)
            upstream.close()
            return

        # 绝对 URI（http:// 或 https://）
        if target.startswith('https://'):
            rest, tls, dport = target[8:], True, 443
        elif target.startswith('http://'):
            rest, tls, dport = target[7:], False, 80
        else:
            rest, tls, dport = target, False, 80
        hostpart, _, path = rest.partition('/')
        path = '/' + path
        host, _, port = hostpart.partition(':')
        port = int(port or dport)
        try:
            upstream = socket.create_connection((host, port), timeout=15)
            if tls:
                ctx = ssl.create_default_context()
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
                upstream = ctx.wrap_socket(upstream, server_hostname=host)
        except Exception as e:
            sys.stderr.write(f'{method} {host}:{port} FAIL {e!r}\n'); sys.stderr.flush()
            self.wfile.write(b'HTTP/1.1 502 Bad Gateway\r\n\r\n')
            return
        hdrs = ''.join(f'{k}: {v}\r\n' for k, v in headers.items()
                       if k.lower() not in ('proxy-connection', 'connection', 'proxy-authorization'))
        req = f'{method} {path} HTTP/1.0\r\n{hdrs}Connection: close\r\n\r\n'
        upstream.sendall(req.encode('latin1'))
        pipe(self.connection, upstream)
        upstream.close()

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8888
    Server(('127.0.0.1', port), Handler).serve_forever()

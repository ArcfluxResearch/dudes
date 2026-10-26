// Serves a built directory with the Floor's strict page policy, so the checks see what a person's browser sees.

import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { extname, join, normalize } from 'node:path';

const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8',
  '.woff2': 'font/woff2', '.json': 'application/json' };

/** No inline script or style, no data: image, nothing from another origin. The fonts and the icon load from the same origin only. */
export const POLICY = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'";

export async function serve(root) {
  const server = createServer(async (request, response) => {
    const path = normalize(new URL(request.url ?? '/', 'http://x').pathname);
    if (path.includes('..')) { response.writeHead(400).end(); return; }
    if (path === '/favicon.ico') { response.writeHead(204).end(); return; }
    try {
      const file = path === '/' ? '/index.html' : path;
      const body = await readFile(join(root, file));
      response.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream', 'content-security-policy': POLICY, 'cache-control': 'no-store' });
      response.end(body);
    } catch {
      response.writeHead(404, { 'content-security-policy': POLICY }).end();
    }
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  return { url: `http://127.0.0.1:${server.address().port}/`, close: () => new Promise((done) => { server.close(done); server.closeAllConnections(); }) };
}

"""A SharePoint Server 2019 REST endpoint behind Windows (NTLM) authentication,
for tests/sharepoint.rs.

The NTLM acceptor is pyspnego's, an implementation independent of the client
under test, checking the credentials in NTLM_USER_FILE ("DOMAIN:USER:PASSWORD").
As IIS does, a connection that completed the handshake stays authenticated.

The REST shapes follow Microsoft's documentation for lists, list items, the
request digest and files, answered as `application/json;odata=nometadata`:
  GET  {site}/_api/web
  POST {site}/_api/contextinfo
  GET  {site}/_api/web/lists/GetByTitle('<list>')/items   ($top, $select, $filter, paging)
  POST {site}/_api/web/lists/GetByTitle('<list>')/items   (needs X-RequestDigest)
  GET  {site}/_api/web/GetFileByServerRelativeUrl('<url>')/$value
  POST {site}/_api/web/GetFolderByServerRelativeUrl('<folder>')/Files/add(url='<name>',overwrite=<bool>)
plus GET /_test/state (no auth) so a test can see exactly what was stored.

Run: NTLM_USER_FILE=users.txt python sharepoint_mock.py 8765
"""

import base64
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

import spnego

SITE = "/sites/team"
DIGEST = "0x5CB1F0DIGEST,24 Sep 2026 10:00:00 -0000"
PAGE_TOKEN = re.compile(r"Paged=TRUE&p_ID=(\d+)")

LOCK = threading.Lock()
STATE = {
    "lists": {
        "Orders": [
            {"Id": i, "ID": i, "Title": t, "Amount": a, "Region": r, "Created": "2026-09-0%dT08:00:00Z" % i}
            for i, (t, a, r) in enumerate(
                [("Zoë's order", 12.5, "North"), ("Beta", 7.0, "South"), ("Gamma", 3.25, "North"),
                 ("Delta", 100.0, "East"), ("Epsilon", 0.5, "North")],
                start=1,
            )
        ],
        "Imported": [],
    },
    "files": {SITE + "/Shared Documents/orders.csv": b"id,name,amount\n1,Zo\xc3\xab,12.5\n2,Ana,7\n"},
    "handshakes": 0,
    "requests": [],
}


def odata_error(code, message):
    return {"odata.error": {"code": code, "message": {"lang": "en-US", "value": message}}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    # --- plumbing -----------------------------------------------------------

    def send(self, status, body=b"", content_type="application/json;odata=nometadata", headers=()):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode("utf-8")
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def read_body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def authenticated(self):
        """True when this connection has completed NTLM. Otherwise answers the
        401 for the next step of the handshake and returns False."""
        if getattr(self, "user", None):
            return True
        header = self.headers.get("Authorization", "")
        scheme, _, token = header.partition(" ")
        if scheme not in ("NTLM", "Negotiate") or not token:
            self.send(401, b"401 UNAUTHORIZED", "text/plain",
                      [("WWW-Authenticate", "Negotiate"), ("WWW-Authenticate", "NTLM")])
            return False
        raw = base64.b64decode(token)
        kind = int.from_bytes(raw[8:12], "little") if raw[:8] == b"NTLMSSP\x00" else 0
        try:
            if kind == 1:
                self.ctx = spnego.server(protocol="ntlm")
            out = self.ctx.step(raw)
        except Exception as e:  # a wrong password lands here
            self.ctx = None
            self.send(401, ("denied: %s" % e).encode(), "text/plain",
                      [("WWW-Authenticate", "Negotiate"), ("WWW-Authenticate", "NTLM")])
            return False
        if not self.ctx.complete:
            self.send(401, b"", "text/plain",
                      [("WWW-Authenticate", "%s %s" % (scheme, base64.b64encode(out).decode()))])
            return False
        with LOCK:
            STATE["handshakes"] += 1
        self.user = self.ctx.client_principal
        return True

    def route(self):
        parts = urlsplit(self.path)
        return unquote(parts.path), parse_qs(parts.query)

    # --- endpoints ----------------------------------------------------------

    def do_GET(self):
        path, query = self.route()
        if path == "/_test/state":
            with LOCK:
                snapshot = {
                    "lists": STATE["lists"],
                    "files": {k: v.decode("utf-8", "replace") for k, v in STATE["files"].items()},
                    "handshakes": STATE["handshakes"],
                    "requests": STATE["requests"],
                }
            return self.send(200, snapshot, "application/json")
        if not self.authenticated():
            return
        with LOCK:
            STATE["requests"].append({"method": "GET", "path": path, "query": query})
        if path == SITE + "/_api/web":
            return self.send(200, {"Title": "Team", "ServerRelativeUrl": SITE})
        m = re.fullmatch(re.escape(SITE) + r"/_api/web/lists/GetByTitle\('((?:[^']|'')*)'\)/items", path)
        if m:
            return self.list_items(m.group(1).replace("''", "'"), query)
        m = re.fullmatch(re.escape(SITE) + r"/_api/web/GetFileByServerRelativeUrl\('((?:[^']|'')*)'\)/\$value", path)
        if m:
            url = m.group(1).replace("''", "'")
            with LOCK:
                data = STATE["files"].get(url)
            if data is None:
                return self.send(404, odata_error("-2130575338, System.IO.FileNotFoundException",
                                                  "File Not Found."))
            return self.send(200, data, "application/octet-stream")
        self.send(404, odata_error("-1, Microsoft.SharePoint.Client.InvalidClientQueryException",
                                   "The expression is not valid."))

    def list_items(self, title, query):
        with LOCK:
            items = STATE["lists"].get(title)
        if items is None:
            return self.send(404, odata_error("-1, System.ArgumentException",
                                              "List '%s' does not exist at site with URL 'http://%s%s'."
                                              % (title, self.headers.get("Host"), SITE)))
        top = int(query.get("$top", ["100"])[0])
        after = 0
        token = query.get("$skiptoken", [""])[0]
        if token:
            after = int(PAGE_TOKEN.search(token).group(1))
        page = [i for i in items if i["Id"] > after][:top]
        if "$select" in query:
            keep = [c.strip() for c in query["$select"][0].split(",")]
            page = [{k: v for k, v in i.items() if k in keep} for i in page]
        body = {"value": page}
        rest = [i for i in items if i["Id"] > after][top:]
        if rest and page:
            last = [i for i in items if i["Id"] > after][top - 1]["Id"]
            host = self.headers.get("Host")
            extra = "".join("&%s=%s" % (k, v[0]) for k, v in query.items() if k not in ("$top", "$skiptoken"))
            body["odata.nextLink"] = (
                "http://%s%s/_api/web/lists/GetByTitle('%s')/items?%%24skiptoken=Paged%%3dTRUE%%26p_ID%%3d%d&%%24top=%d%s"
                % (host, SITE, title.replace("'", "''"), last, top, extra)
            )
        self.send(200, body)

    def do_POST(self):
        path, _ = self.route()
        body = self.read_body()
        if not self.authenticated():
            return
        with LOCK:
            STATE["requests"].append({"method": "POST", "path": path, "bytes": len(body)})
        if path == SITE + "/_api/contextinfo":
            return self.send(200, {"FormDigestTimeoutSeconds": 1800, "FormDigestValue": DIGEST,
                                   "LibraryVersion": "16.0.10337.12109", "SiteFullUrl": SITE,
                                   "WebFullUrl": SITE})
        if self.headers.get("X-RequestDigest") != DIGEST:
            return self.send(403, odata_error("-2130575251, Microsoft.SharePoint.SPException",
                                              "The security validation for this page is invalid and might be "
                                              "corrupted. Please use your web browser's Back button to try your "
                                              "operation again."))
        m = re.fullmatch(re.escape(SITE) + r"/_api/web/lists/GetByTitle\('((?:[^']|'')*)'\)/items", path)
        if m:
            title = m.group(1).replace("''", "'")
            with LOCK:
                items = STATE["lists"].get(title)
                if items is None:
                    return self.send(404, odata_error("-1, System.ArgumentException",
                                                      "List '%s' does not exist." % title))
                item = json.loads(body.decode("utf-8"))
                item["Id"] = item["ID"] = len(items) + 1
                items.append(item)
            return self.send(201, item)
        m = re.fullmatch(re.escape(SITE) + r"/_api/web/GetFolderByServerRelativeUrl\('((?:[^']|'')*)'\)"
                         r"/Files/add\(url='((?:[^']|'')*)',overwrite=(true|false)\)", path)
        if m:
            folder, name = m.group(1).replace("''", "'"), m.group(2).replace("''", "'")
            url = folder.rstrip("/") + "/" + name
            with LOCK:
                if url in STATE["files"] and m.group(3) == "false":
                    return self.send(400, odata_error("-2130575257, Microsoft.SharePoint.SPException",
                                                      "A file with the name %s already exists." % url))
                STATE["files"][url] = body
            return self.send(200, {"Name": name, "ServerRelativeUrl": url, "Length": str(len(body))})
        self.send(404, odata_error("-1, Microsoft.SharePoint.Client.InvalidClientQueryException",
                                   "The expression is not valid."))


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()

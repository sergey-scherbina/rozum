#!/usr/bin/env python3
# Inject PWA <head> tags into the ssc-generated control-center index.html (idempotent).
import sys
p = sys.argv[1] if len(sys.argv) > 1 else __import__('os').path.expanduser("~/.rozum/ucc/site/index.html")
h = open(p).read()
if 'rel="manifest"' in h:
    print("already injected"); sys.exit(0)
tags = (
 '<link rel="manifest" href="/manifest.webmanifest">'
 '<meta name="theme-color" content="#2563eb">'
 '<meta name="apple-mobile-web-app-capable" content="yes">'
 '<meta name="mobile-web-app-capable" content="yes">'
 '<meta name="apple-mobile-web-app-status-bar-style" content="default">'
 '<meta name="apple-mobile-web-app-title" content="rozum control">'
 '<link rel="icon" type="image/svg+xml" href="/icon.svg">'
 '<link rel="apple-touch-icon" href="/icon-180.png">'
)
h = h.replace('</head>', tags + '</head>', 1)   # first (outer real) head only
open(p, 'w').write(h)
print("injected PWA head tags")

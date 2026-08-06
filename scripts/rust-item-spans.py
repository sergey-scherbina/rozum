"""Find the line span of each top-level Rust item — for planning and performing extractions.

    python3 scripts/rust-item-spans.py <file.rs> ['<name-regex>']

With a regex it reports that family as one candidate module: how many items, how many lines, and
what it calls outside itself. Used to plan the `gw-monolith-decompose` slices; the three comments
below are bugs it had, each of which produced a WRONG answer that looked right.

The naive version closed an item's span on the first line whose brace depth returned to zero.
For `async fn f(\n  arg,\n) -> R {` that line has `(` and no `{`, so the span was ONE line and the
body was left orphaned. An item ends only after its first `{` has been opened and matched — or at
the first `;` for brace-less items (`type`, `const`, `use`).
"""
import re, sys, pathlib

def braces(line):
    """(net depth change, number of opening braces) outside strings and line comments.

    Both are needed: a ONE-LINE item (`enum E { A, B }`) has net 0 but did open a body, and a
    signal that only watches net depth never notices it started — so its span ran on and
    swallowed the item after it."""
    out, opens, i, n, q = 0, 0, 0, len(line), False
    while i < n:
        ch = line[i]
        if not q and ch == '/' and i + 1 < n and line[i+1] == '/':
            break
        if ch == '"':
            q = not q
        elif not q and ch == "'":
            # A CHAR LITERAL, not a lifetime: `'{'` in `body.starts_with('{')` is a brace to the
            # naive counter, so the span never closed and swallowed 2388 lines. `'a` in `&'a str`
            # is not a literal and must not be skipped, so only skip when the quote closes.
            j = i + 1
            if j < n and line[j] == '\\':
                j += 1
            if j + 1 < n and line[j + 1] == "'":
                i = j + 1
        elif not q:
            if ch == '{':
                out += 1; opens += 1
            elif ch == '}':
                out -= 1
        i += 1
    return out, opens

# A leading attribute may sit on the SAME line as the item — `#[derive(Deserialize)] struct Foo {…}`
# is common for one-line request DTOs, and anchoring at the item keyword alone silently skips them.
ITEM = re.compile(r'^(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))? )?(?:async )?(fn|struct|enum|const|static|type|impl)\b\s*([A-Za-z_]\w*)?')

def spans(src, want):
    found, i = [], 0
    while i < len(src):
        m = ITEM.match(src[i])
        if m and m.group(2) and want(m.group(2)):
            d, j, opened = 0, i, False
            while j < len(src):
                net, opens = braces(src[j])
                d += net
                opened = opened or opens > 0
                if opened and d == 0:
                    break
                if not opened and src[j].rstrip().endswith(";"):
                    break                      # brace-less item
                j += 1
            k = i
            while k > 0 and (src[k-1].lstrip().startswith("//") or src[k-1].lstrip().startswith("#[")):
                k -= 1
            # An item that never closes — the last one in a file, or a mis-parse — leaves `j` one
            # past the end, and the caller indexes with it. Clamp rather than emit an impossible
            # span: an out-of-range crash on a bigger pattern is how this was found.
            found.append((k, min(j, len(src) - 1), m.group(2)))
            i = j + 1
        else:
            i += 1
    return found


def _report(path, pattern=None):
    src = pathlib.Path(path).read_text().split("\n")
    defined = {m.group(1) for m in
               (re.match(r'^(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))? )?(?:async )?(?:fn|struct|enum|const|static|type) ([A-Za-z_]\w*)', l)
                for l in src) if m}
    if pattern is None:
        got = spans(src, lambda n: True)
        print(f"{path}: {len(src)} lines, {len(got)} top-level items")
        big = sorted(((b - a + 1, n) for a, b, n in got), reverse=True)[:15]
        print(f"{'item':34}{'lines':>7}")
        for size, name in big:
            print(f"{name:34}{size:7}")
        return
    pat = re.compile(pattern)
    got = spans(src, lambda n: bool(pat.search(n)))
    lines = set()
    for a, b, _ in got:
        lines |= set(range(a, b + 1))
    # Comments are not code: `status` inside "errors land in the row's status" was reported as a
    # dependency twice, on two different families, before this line existed. Strip line comments
    # before looking for calls.
    inside = "\n".join(re.sub(r'//.*$', '', src[i]) for i in sorted(lines))
    fam = {n for _, _, n in got}
    # CALLS, not bare names: a bare-name match reports every identifier that merely appears,
    # which made `status` and `serve` look like dependencies of a family that never calls them.
    calls = sorted(d for d in defined - fam
                   if re.search(rf'(?<![\w.]){re.escape(d)}\s*\(', inside))
    used_outside = sorted(n for n in fam
                          if re.search(rf'\b{re.escape(n)}\b',
                                       "\n".join(l for i, l in enumerate(src) if i not in lines)))
    print(f"family /{pattern}/ in {path}")
    print(f"  items {len(fam)}   lines {len(lines)} ({len(lines) * 100 // max(len(src), 1)}% of file)")
    print(f"  calls out ({len(calls)}): {', '.join(calls) or 'nothing'}")
    print(f"  named from outside ({len(used_outside)}): {', '.join(used_outside) or 'nothing'}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    _report(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)

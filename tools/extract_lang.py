"""Collects every translatable string from the source into a .lang file.

Run from the repository root:

    python tools/extract_lang.py lang/template.lang

The keys are the German originals as they appear in `t(...)` and `tf(...)`, so
a translator only ever edits the right-hand side. Rerunning over a file that
already exists keeps every translation in it and appends whatever is new:

    python tools/extract_lang.py lang/en.lang

The file is always written as UTF-8 — redirecting stdout on Windows would hand
it to the ANSI codepage and mangle every umlaut.
"""
import io
import os
import re
import sys

# A Rust literal may run over several lines: a trailing backslash swallows the
# newline and the indent that follows. The escape class therefore has to accept
# a newline after the backslash, which `.` does not without DOTALL.
CALL = re.compile(
    r'\b(?:t|tf|crate::i18n::t|crate::i18n::tf)\(\s*'
    r'("(?:[^"\\]|\\[\s\S])*")',
    re.S,
)


# Test modules carry throwaway strings that must not reach a translator.
TEST_MOD = re.compile(r"\n#\[cfg\(test\)\]\nmod tests \{.*?\n\}\n", re.S)


# The file-type groups carry their label in a table rather than in a `t(...)`
# call, but the chips show it, so it has to reach the translator too.
LABEL = re.compile(r'^\s*label:\s*("(?:[^"\\]|\\.)*")\s*,', re.M)


def sources():
    for root, _dirs, files in os.walk("src"):
        for f in files:
            if f.endswith(".rs"):
                yield os.path.join(root, f)


def join_continuations(lit):
    """Rust splits long literals with a trailing backslash; rebuild the text."""
    parts = re.findall(r'"((?:[^"\\]|\\[\s\S])*)"', lit, re.S)
    text = "".join(parts)
    # A `\` at end of line swallows the newline and the following indent.
    text = re.sub(r"\\\s*\n\s*", "", text)
    return text


def unescape(s):
    """Turns a Rust literal body into the text the program actually shows."""
    out = []
    i = 0
    while i < len(s):
        c = s[i]
        if c != "\\" or i + 1 >= len(s):
            out.append(c)
            i += 1
            continue
        nxt = s[i + 1]
        # `\x20` is how a literal keeps a leading space that a line
        # continuation would otherwise swallow — the help text is full of them.
        if nxt == "x" and i + 3 < len(s):
            try:
                out.append(chr(int(s[i + 2:i + 4], 16)))
                i += 4
                continue
            except ValueError:
                pass
        out.append({"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\", "'": "'"}.get(nxt, "\\" + nxt))
        i += 2
    return "".join(out)


def escape(s):
    # `=` separates key from value, so one inside the key has to be escaped —
    # "Klick = hineinzoomen" would otherwise be cut in half on load.
    return (
        s.replace("\\", "\\\\")
        .replace("\n", "\\n")
        .replace("\t", "\\t")
        .replace("=", "\\=")
    )


def split_pair(line):
    """Splits at the first `=` that is not escaped. Mirrors the Rust loader."""
    i = 0
    while i < len(line):
        if line[i] == "\\":
            i += 2
            continue
        if line[i] == "=":
            return line[:i].strip(), line[i + 1:].strip()
        i += 1
    return None


def load_existing(path):
    out = {}
    if not path or not os.path.isfile(path):
        return out
    for line in io.open(path, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        pair = split_pair(line)
        if pair:
            out[pair[0]] = pair[1]
    return out


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: python tools/extract_lang.py <lang/xx.lang>")
    target = sys.argv[1]
    existing = load_existing(target)
    seen = []
    known = set()
    for path in sorted(sources()):
        src = TEST_MOD.sub("\n", io.open(path, encoding="utf-8").read())
        for m in list(CALL.finditer(src)) + list(LABEL.finditer(src)):
            text = unescape(join_continuations(m.group(1)))
            if not text or text in known:
                continue
            known.add(text)
            seen.append(text)

    lines = [
        "# Diskalize interface language file.",
        "# Left of the '=' is the German original and must not be changed.",
        "# Leave a line blank or delete it to keep the German wording.",
        "# Use \\n for a line break. {0} {1} ... are values filled in at runtime",
        "# and may be reordered, but every one of them has to stay.",
        "@name = " + existing.get("@name", "LANGUAGE NAME HERE"),
        "",
    ]
    translated = 0
    for text in seen:
        key = escape(text)
        value = existing.get(key, "")
        translated += bool(value)
        lines.append("{} = {}".format(key, value))
    io.open(target, "w", encoding="utf-8", newline="\n").write("\n".join(lines) + "\n")
    print("{}: {} strings, {} translated".format(target, len(seen), translated))


main()

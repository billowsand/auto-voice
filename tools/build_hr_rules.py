#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Generate ``models/hr/replace.fst`` for sherpa-onnx's HomophoneReplacer
(``hr_rule_fsts`` config).

The simplest way to add words: edit ``user-words.txt`` and put **one correct
Chinese word per line** — no pinyin needed. The script looks every character up
in the sherpa-onnx ``lexicon.txt`` and derives the tone-numbered pinyin itself:

    # user-words.txt
    玄戒
    玄戒芯片

then rerun:

    tools/.venv/Scripts/python.exe tools/build_hr_rules.py

Requires ``kaldifst`` — the same OpenFst build sherpa-onnx links against, so
the binary format is correct by construction:

    pip install kaldifst

kaldifst only publishes wheels up to CPython 3.13. On a newer interpreter pip
falls back to a source build and fails; use a 3.11–3.13 environment instead
(this repo keeps one at ``tools/.venv``).


Rule syntax (three equivalent forms, picked per line)
------------------------------------------------------
1. Chinese word only (recommended)::

       玄戒

   The pinyin is looked up character by character in ``--lexicon``
   (default ``models/hr/lexicon.txt``). Every Chinese character must exist in
   the lexicon. The lookup yields each character's *primary* reading, so a
   polyphone whose intended reading is not the primary one needs form 2.

2. pinyin<TAB>Chinese (or pinyin<spaces>Chinese) — explicit pinyin, for
   polyphones / unusual readings / overrides::

       xuan2jie4\t玄戒
       le4shi2 乐事            # force a non-primary reading

3. pynini style (kept for compatibility with the sherpa-onnx colab recipe)::

       pynini.cross("xuan2jie4", "玄戒")

The match side is always pinyin and must be ASCII; the replacement side must be
non-ASCII (Chinese). Lines that cannot be honoured are skipped with a warning.


How the rule FST is used at runtime
-----------------------------------
``kaldifst::TextNormalizer::Normalize(words, pronunciations)`` builds a linear
transducer whose **input** side is the UTF-8 bytes of the recognized Chinese
characters and whose **output** side is the UTF-8 bytes of their pinyin (the
two are aligned per word and zero-padded to the longer of the pair). It then
composes that with our FST and takes the shortest path.

So composition matches *pinyin* bytes against our FST's input side, and our
output side supplies the replacement characters:

    hanzi ──(text FST)──> pinyin ──(replace.fst)──> hanzi

Reconstruction is done by ``FstToString2``, which walks the one-best path and,
per arc:

    olabel == 0     -> emit nothing
    olabel <  128   -> emit the *ilabel* (the original character byte)
    olabel >= 128   -> emit the olabel  (our replacement byte)

That is why an unmatched span round-trips back to the original text, and why
replacements must be non-ASCII — see ``check_rules`` below.


Why not pynini
--------------
The upstream recipe is ``cdrewrite(rules, "", "", sigma)``, but pynini has no
Windows wheel and does not build under MSVC. We get the same effect with a
weighted "optional rewrite" machine, which is a few lines of AT&T text format:

    * a hub state (start + final) carrying a ``b:b`` self-loop for every byte,
      each costing COPY_COST — this is the passthrough / sigma-closure part
    * one zero-cost chain per rule, leaving the hub and returning to it

Every arc has non-negative weight, so ShortestPath is well defined, and a rule
chain is always cheaper than copying the same span byte by byte. That makes
rule application obligatory and, between overlapping rules, picks the longest
match — the behaviour cdrewrite gives.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import Dict, List, Tuple

try:
    import kaldifst
except ImportError:  # pragma: no cover - environment problem, not logic
    sys.exit(
        "error: kaldifst is not installed.\n"
        "       pip install kaldifst        (needs CPython 3.8-3.13)\n"
        "       or run this script with tools/.venv/Scripts/python.exe"
    )

# ── Defaults ────────────────────────────────────────────────────────────────
ROOT = Path(__file__).resolve().parent.parent  # repo root
DEFAULT_WORDS = ROOT / "tools" / "user-words.txt"
DEFAULT_LEXICON = ROOT / "models" / "hr" / "lexicon.txt"
DEFAULT_OUT = ROOT / "models" / "hr" / "replace.fst"

# Cost of copying one byte through unchanged. Rule arcs cost 0, so any rule is
# preferred over passthrough, and a longer rule is preferred over a shorter one
# that covers the same prefix.
COPY_COST = 1.0

Rule = Tuple[str, str]

# A run of CJK characters — used to detect the "Chinese word only" form.
_CJK_RE = re.compile(r"[一-鿿]")


# ── Lexicon (hanzi -> tone-numbered pinyin) ─────────────────────────────────


def load_lexicon(path: Path) -> Dict[str, str]:
    """Build a ``{single_hanzi: pinyin}`` map from sherpa-onnx's lexicon.

    The file is ``word<TAB-or-spaces>pinyin pinyin ...`` per line. We keep only
    single-character words (which carry exactly one reading) so each hanzi maps
    to its primary tone-numbered pinyin. Multi-character entries are ignored —
    whole words are assembled by concatenating per-character readings.
    """
    table: Dict[str, str] = {}
    if not path.exists():
        return table

    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        word, readings = parts[0], parts[1:]
        # Keep a single primary reading per character; first occurrence wins.
        if len(word) == 1 and word not in table:
            table[word] = readings[0]
    return table


def lookup_pinyin(word: str, lexicon: Dict[str, str]) -> Tuple[str, List[str]]:
    """Return ``(pinyin, missing)`` for a Chinese word looked up per character.

    ``pinyin`` is the concatenation of each character's tone-numbered reading
    (spaces stripped), matching the byte stream sherpa-onnx composes against.
    ``missing`` lists characters absent from the lexicon.
    """
    syllables: List[str] = []
    missing: List[str] = []
    for ch in word:
        if ch.isspace():
            continue
        reading = lexicon.get(ch)
        if reading is None:
            missing.append(ch)
        else:
            syllables.append(reading)
    return "".join(syllables), missing


# ── Rules ──────────────────────────────────────────────────────────────────


def _split_pinyin_chinese(line: str) -> Tuple[str, str] | None:
    """Parse ``pinyin<whitespace>Chinese`` (explicit-pinyin form).

    Returns ``(src, dst)`` or ``None`` if the line does not look like
    "ASCII pinyin followed by whitespace followed by something".
    """
    m = re.match(r"^(\S+)\s+(.+)$", line)
    if not m:
        return None
    src, dst = m.group(1).strip(), m.group(2).strip()
    # Only treat the leading token as pinyin if it is pure ASCII letters/digits;
    # otherwise the line is probably a bare Chinese word containing no space.
    if not re.fullmatch(r"[A-Za-z0-9]+", src):
        return None
    return src, dst


def _parse_pynini_cross(line: str) -> Tuple[str, str] | None:
    """Parse ``pynini.cross('src', 'dst')`` — accept either kind of quote."""
    chosen: List[str] = []
    for q in ("'", '"'):
        i = 0
        chosen = []
        while len(chosen) < 2:
            a = line.find(q, i)
            if a < 0:
                break
            b = line.find(q, a + 1)
            if b < 0:
                break
            chosen.append(line[a + 1 : b])
            i = b + 1
        if len(chosen) == 2:
            break
    if len(chosen) != 2 or not chosen[0]:
        return None
    return chosen[0], chosen[1]


def load_rules(words_file: Path, lexicon: Dict[str, str]) -> List[Rule]:
    """Read ``user-words.txt`` into ``(pinyin, chinese)`` rules.

    Each non-blank, non-``#`` line is one of:
        CHINESE_ONLY    玄戒                     (pinyin derived from lexicon)
        EXPLICIT        xuan2jie4<TAB>玄戒       (whitespace-separated)
        CROSS_STYLE     pynini.cross("xuan2jie4", "玄戒")
    """
    if not words_file.exists():
        return []

    rules: List[Rule] = []
    for lineno, raw in enumerate(
        words_file.read_text(encoding="utf-8").splitlines(), start=1
    ):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue

        # pynini style (check first: it may contain spaces inside quotes).
        if "cross" in line:
            parsed = _parse_pynini_cross(line)
            if parsed:
                rules.append(parsed)
            else:
                print(f"  skip line {lineno} (unparsable cross): {line!r}",
                      file=sys.stderr)
            continue

        # Explicit pinyin + Chinese (whitespace separated).
        parsed = _split_pinyin_chinese(line)
        if parsed:
            rules.append(parsed)
            continue

        # Bare Chinese word(s): derive pinyin from the lexicon.
        if _CJK_RE.search(line):
            word = "".join(line.split())  # drop any internal whitespace
            pinyin, missing = lookup_pinyin(word, lexicon)
            if missing:
                print(
                    f"  skip line {lineno} {word!r}: character(s) "
                    f"{''.join(missing)!r} not in lexicon; add explicit pinyin "
                    f"(e.g. 'pin1yin1 {word}')",
                    file=sys.stderr,
                )
                continue
            rules.append((pinyin, word))
            continue

        print(f"  skip line {lineno} (unparsable): {line!r}", file=sys.stderr)

    return rules


def check_rules(rules: List[Rule]) -> List[Rule]:
    """Drop rules the runtime cannot honour, warning about each."""
    seen: dict[str, str] = {}
    ok: List[Rule] = []

    for src, dst in rules:
        if not src:
            print("  skip (empty pinyin)", file=sys.stderr)
            continue

        if not src.isascii():
            print(
                f"  skip {src!r}: the match side is pinyin and must be ASCII",
                file=sys.stderr,
            )
            continue

        # FstToString2 emits the *original* character whenever our output byte
        # is ASCII, so an ASCII replacement silently does nothing.
        ascii_chars = sorted({c for c in dst if c.isascii()})
        if ascii_chars:
            print(
                f"  skip {src!r} -> {dst!r}: replacement contains ASCII "
                f"{ascii_chars} which sherpa-onnx cannot emit",
                file=sys.stderr,
            )
            continue

        if src in seen and seen[src] != dst:
            print(
                f"  skip {src!r} -> {dst!r}: already mapped to {seen[src]!r} "
                f"(same pinyin, different hanzi — keeping the first)",
                file=sys.stderr,
            )
            continue

        seen[src] = dst
        ok.append((src, dst))

    return ok


# ── FST construction ───────────────────────────────────────────────────────


def build_rule_fst_text(rules: List[Rule]) -> str:
    """Emit the AT&T text format for the rewrite transducer.

    State 0 is the hub: it is the start state, the only final state, and it
    carries the byte-copy self-loops. Each rule is a chain of zero-cost arcs
    leaving the hub and returning to it.
    """
    lines: List[str] = []

    # Passthrough. Label 0 is epsilon in OpenFst, so bytes start at 1.
    for b in range(1, 256):
        lines.append(f"0 0 {b} {b} {COPY_COST}")

    next_state = 1
    for src, dst in rules:
        in_bytes = src.encode("utf-8")
        out_bytes = dst.encode("utf-8")
        # Pad the shorter side with epsilon (label 0). A longer replacement
        # yields input-epsilon arcs, which compose fine — they just consume
        # nothing from the pinyin stream.
        n = max(len(in_bytes), len(out_bytes))

        state = 0
        for k in range(n):
            ilabel = in_bytes[k] if k < len(in_bytes) else 0
            olabel = out_bytes[k] if k < len(out_bytes) else 0
            if k == n - 1:
                dest = 0  # back to the hub
            else:
                dest = next_state
                next_state += 1
            lines.append(f"{state} {dest} {ilabel} {olabel} 0.0")
            state = dest

    lines.append("0")  # hub is final, weight 0
    return "\n".join(lines)


def write_fst(rules: List[Rule], out_path: Path) -> None:
    fst = kaldifst.compile(build_rule_fst_text(rules), acceptor=False)
    kaldifst.arcsort(fst, sort_type="ilabel")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    fst.write(str(out_path))

    size = out_path.stat().st_size
    print(f"  wrote {out_path} ({size} bytes, {fst.num_states} states)")


# ── Self-test ──────────────────────────────────────────────────────────────


def self_test(rules: List[Rule], out_path: Path) -> bool:
    """Replay every rule through the real runtime code path.

    ``TextNormalizer`` here is the same class sherpa-onnx calls, so a pass
    means auto-voice will load and apply the file.
    """
    tn = kaldifst.TextNormalizer(str(out_path))
    failures = 0

    # A placeholder stands in for whatever characters the ASR produced; the
    # replacement path discards them anyway.
    placeholder = "〇"

    for src, dst in rules:
        got = tn.normalize([placeholder], [src])
        if got != dst:
            print(f"  FAIL {src!r}: expected {dst!r}, got {got!r}", file=sys.stderr)
            failures += 1

    # Unmatched input must round-trip back to the original characters.
    got = tn.normalize(["你", "好"], ["ni3", "hao3"])
    if got != "你好":
        print(f"  FAIL passthrough: expected '你好', got {got!r}", file=sys.stderr)
        failures += 1

    if failures:
        print(f"  {failures} self-test failure(s)", file=sys.stderr)
        return False

    print(f"  self-test passed ({len(rules)} rule(s) + passthrough)")
    return True


# ── Main ───────────────────────────────────────────────────────────────────


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--words", type=Path, default=DEFAULT_WORDS,
                    help=f"Path to user-words.txt (default: {DEFAULT_WORDS})")
    ap.add_argument("--lexicon", type=Path, default=DEFAULT_LEXICON,
                    help="sherpa-onnx lexicon for hanzi->pinyin lookup "
                         f"(default: {DEFAULT_LEXICON})")
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT,
                    help=f"Path to write replace.fst (default: {DEFAULT_OUT})")
    ap.add_argument("--no-self-test", action="store_true",
                    help="Skip replaying the rules through kaldifst")
    args = ap.parse_args()

    print(f"Reading rules from {args.words}")
    lexicon = load_lexicon(args.lexicon)
    if not lexicon:
        print(
            f"  WARNING: lexicon not found/empty at {args.lexicon}; "
            "Chinese-only lines will be skipped.",
            file=sys.stderr,
        )
    else:
        print(f"  lexicon: {len(lexicon)} characters from {args.lexicon}")

    rules = check_rules(load_rules(args.words, lexicon))
    print(f"  {len(rules)} rule(s) loaded")
    for src, dst in rules:
        print(f"    {src:24s} -> {dst}")

    if not rules:
        print(
            "  WARNING: no rules. The FST will pass text through unchanged.",
            file=sys.stderr,
        )

    write_fst(rules, args.out)

    if not args.no_self_test and not self_test(rules, args.out):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

//! The options a filter may take after a command that runs unasked — a check's (`tools::FILTERS`, #32) or
//! one allowed with a run card's `a` (`tools::PIPE_FILTERS`, #193): an allow-list per program, read as
//! getopt_long reads it. Every other option is refused, whatever it does — a list of what's harmful misses
//! a spelling (`sort --compress-prog=./x`, `--out=f`, `tail --fol`: any unambiguous start of a long name is
//! that option).

/// One filter's options. Long names: `name` takes no value, `name=` one (`--name=v`, or the next word),
/// `name?` one only after `=` (the next word is never taken as its value, so it's judged on its own).
pub struct Spec {
    pub program: &'static str,
    /// Short letters that take no value — clustered (`-rn`); digits are a count (`head -5`, `grep -3`)
    flags: &'static str,
    /// Short letters that take a value: the cluster's rest (`-n5`), else the next word (`-n 5`)
    valued: &'static str,
    long: &'static [&'static str],
    /// The program's other long names: never allowed, listed so a prefix resolves as the program resolves it
    /// (an exact name wins; a prefix of several names is ambiguous — refused)
    refused: &'static [&'static str],
    /// The most words that aren't options (`uniq in out` writes `out`); None: any — what they name is read
    /// as any read is (`readonly::outside`)
    operands: Option<usize>,
}

// GNU's and the BSDs' (macOS) options alike: allowed only where it reads its input and says something.
const SPECS: [Spec; 8] = [
    Spec {
        program: "grep",
        flags: "EFGPiywxvclLoqsbHhnTzaIU0123456789",
        valued: "eABCm",
        long: &[
            "extended-regexp",
            "fixed-strings",
            "basic-regexp",
            "perl-regexp",
            "ignore-case",
            "no-ignore-case",
            "word-regexp",
            "line-regexp",
            "null-data",
            "invert-match",
            "count",
            "files-with-matches",
            "files-without-match",
            "only-matching",
            "quiet",
            "silent",
            "no-messages",
            "byte-offset",
            "with-filename",
            "no-filename",
            "line-number",
            "initial-tab",
            "null",
            "text",
            "line-buffered",
            "no-group-separator",
            "binary",
            "regexp=",
            "max-count=",
            "after-context=",
            "before-context=",
            // (GNU's needs a value, BSD's takes one only after `=`: the next word is judged on its own)
            "context?",
            "label=",
            "group-separator=",
            "binary-files=",
            "color?",
            "colour?",
        ],
        refused: &[
            "file",
            "recursive",
            "dereference-recursive",
            "directories",
            "devices",
            "include",
            "include-dir",
            "exclude",
            "exclude-from",
            "exclude-dir",
            "bz2decompress",
            "decompress",
            "lzma",
            "xz",
            "mmap",
            "unix-byte-offsets",
            "help",
            "version",
        ],
        operands: None,
    },
    Spec {
        program: "head",
        flags: "qvz0123456789",
        valued: "nc",
        long: &["quiet", "silent", "verbose", "zero-terminated", "lines=", "bytes="],
        refused: &["help", "version"],
        operands: None,
    },
    Spec {
        program: "tail",
        // (`-r`: BSD's reversed; no `-f`/`-F` — they never end)
        flags: "qvzr0123456789",
        valued: "ncb",
        long: &["quiet", "silent", "verbose", "zero-terminated", "lines=", "bytes="],
        refused: &[
            "follow",
            "retry",
            "pid",
            "sleep-interval",
            "max-unchanged-stats",
            "debug",
            "help",
            "version",
        ],
        operands: None,
    },
    Spec {
        program: "cut",
        flags: "nszw",
        valued: "bcfd",
        long: &[
            "complement",
            "only-delimited",
            "zero-terminated",
            "bytes=",
            "characters=",
            "fields=",
            "delimiter=",
            "output-delimiter=",
        ],
        refused: &["help", "version"],
        operands: None,
    },
    Spec {
        program: "wc",
        flags: "clmwL",
        valued: "",
        long: &["bytes", "chars", "lines", "words", "max-line-length", "total="],
        refused: &["files0-from", "libxo", "help", "version"],
        operands: None,
    },
    Spec {
        program: "sort",
        flags: "bdfgiMhnRrVcCmsuz",
        valued: "ktS",
        long: &[
            "ignore-leading-blanks",
            "dictionary-order",
            "ignore-case",
            "general-numeric-sort",
            "ignore-nonprinting",
            "month-sort",
            "human-numeric-sort",
            "numeric-sort",
            "random-sort",
            "reverse",
            "version-sort",
            "merge",
            "stable",
            "unique",
            "zero-terminated",
            "debug",
            "key=",
            "field-separator=",
            "buffer-size=",
            "parallel=",
            "sort=",
            "check?",
        ],
        refused: &[
            "output",
            "compress-program",
            "temporary-directory",
            "files0-from",
            "random-source",
            "radixsort",
            "mergesort",
            "qsort",
            "heapsort",
            "mmap",
            "batch-size",
            "nthreads",
            "help",
            "version",
        ],
        operands: None,
    },
    Spec {
        program: "uniq",
        flags: "cdDiuz",
        valued: "fsw",
        long: &[
            "count",
            "repeated",
            "ignore-case",
            "unique",
            "zero-terminated",
            "skip-fields=",
            "skip-chars=",
            "check-chars=",
            "all-repeated?",
            "group?",
        ],
        refused: &["help", "version"],
        // (its input; a second is the file it writes — `-` too is an operand, the input)
        operands: Some(1),
    },
    Spec {
        program: "tr",
        flags: "cCdstu",
        valued: "",
        long: &["complement", "delete", "squeeze-repeats", "truncate-set1"],
        refused: &["help", "version"],
        operands: None,
    },
];

/// Whether `words` (the program first) is one of the filters with only options its list allows.
pub fn reads(words: &[&str]) -> bool {
    let Some((&program, args)) = words.split_first() else { return false };
    SPECS.iter().find(|s| s.program == program).is_some_and(|s| takes(s, args))
}

fn takes(spec: &Spec, args: &[&str]) -> bool {
    let mut operands = 0;
    let mut ended = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        i += 1;
        if ended || a == "-" || !a.starts_with('-') {
            operands += 1;
            continue;
        }
        if a == "--" {
            ended = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (typed, value) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v)));
            let Some(option) = resolve(spec, typed) else { return false };
            match (option.as_bytes().last(), value) {
                (Some(b'='), None) if i < args.len() => i += 1,
                (Some(b'='), None) => return false,
                (Some(b'=' | b'?'), Some(_)) | (Some(b'?'), None) => {}
                (_, Some(_)) => return false,
                _ => {}
            }
            continue;
        }
        for (at, c) in a.char_indices().skip(1) {
            if spec.flags.contains(c) {
                continue;
            }
            if !spec.valued.contains(c) {
                return false;
            }
            // (the cluster's rest is its value; none — the next word)
            if at + c.len_utf8() == a.len() {
                if i == args.len() {
                    return false;
                }
                i += 1;
            }
            break;
        }
    }
    spec.operands.is_none_or(|most| operands <= most)
}

/// The long option `typed` names (with its `=`/`?`), if it's one the list allows: an exact name, else the one
/// name it starts.
fn resolve(spec: &Spec, typed: &str) -> Option<&'static str> {
    if typed.is_empty() {
        return None;
    }
    let bare = |o: &str| o.trim_end_matches(['=', '?']).to_string();
    let named: Vec<(&'static str, bool)> = spec
        .long
        .iter()
        .map(|&o| (o, true))
        .chain(spec.refused.iter().map(|&o| (o, false)))
        .filter(|(o, _)| bare(o).starts_with(typed))
        .collect();
    let (option, allowed) = match named.iter().find(|(o, _)| bare(o) == typed) {
        Some(&exact) => exact,
        None if named.len() == 1 => named[0],
        None => return None,
    };
    allowed.then_some(option)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads_line(line: &str) -> bool {
        reads(&line.split_whitespace().collect::<Vec<_>>())
    }

    /// Each filter with the options it reads with — short, clustered, valued, long, a long one's start.
    #[test]
    fn filters_read_with_their_listed_options() {
        for c in [
            "grep A",
            "grep -E a|b",
            "grep -in x",
            "grep -vi x",
            "grep -A3 x",
            "grep -A 3 x",
            "grep -e -x",
            "grep -- -x",
            "grep -3 x",
            "grep --ignore-case x",
            "grep --ign x",
            "grep --max-count=3 x",
            "grep --max-count 3 x",
            "grep --color=always x",
            "grep --context=2 x",
            "head -5",
            "head -n 20",
            "head -n -5",
            "head --lines=3",
            "head --li=3",
            "tail -n +5",
            "tail -20",
            "tail --lines 3",
            "cut -d: -f1",
            "cut -d , -f 2",
            "cut --fields=1",
            "wc -l",
            "wc --lines",
            "sort -rn",
            "sort -t: -k2,2n",
            "sort --check --numeric-sort",
            "sort --rev",
            "sort -u -",
            "uniq -c",
            "uniq -c in.txt",
            "uniq -",
            "uniq --skip-fields 2",
            "tr a-z A-Z",
            "tr -d \\r",
        ] {
            assert!(reads_line(c), "{c}");
        }
    }

    /// Anything else is refused: what writes or runs (in any spelling getopt takes), never ends, reads a file
    /// of its own, an option not on the list, an ambiguous start, a value missing.
    #[test]
    fn filters_refuse_every_option_not_listed() {
        for c in [
            // sort's writer and runner, as typed and as getopt takes their starts
            "sort -o out",
            "sort -ro out",
            "sort --output=out",
            "sort --out=f",
            "sort --o f",
            "sort --compress-program=sh",
            "sort --compress-prog=./x",
            "sort --co ./x",
            "sort -T /x",
            "sort --temporary-directory=/x",
            "sort --files0-from=list",
            "sort --random-source=/x",
            // (`--c`: check or compress-program — ambiguous)
            "sort --c",
            // tail that never ends
            "tail -f",
            "tail -F",
            "tail -5f",
            "tail -qf",
            "tail --follow",
            "tail --fol",
            "tail --f=name",
            "tail --retry",
            "tail --pid=1",
            // grep reading files of its own
            "grep -f pats",
            "grep --file=pats",
            "grep --fi=pats",
            "grep -r x",
            "grep --recursive x",
            "grep --rec x",
            "grep -d recurse x",
            "grep --directories=recurse x",
            "grep --include=*.rs x",
            "grep --exclude-from=x y",
            // (`--line`: line-buffered, line-number or line-regexp)
            "grep --line x",
            "grep --ignore-case=x y",
            "grep -A",
            "wc --files0-from=list",
            "wc --libxo=json",
            "uniq in out",
            "uniq - out",
            "uniq -c - out",
            "cut --complement=x",
            "head --help",
            "tr --version",
            "jq .",
            "sed -n 1p",
        ] {
            assert!(!reads_line(c), "{c}");
        }
    }
}

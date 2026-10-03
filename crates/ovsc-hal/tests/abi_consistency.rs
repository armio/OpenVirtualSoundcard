//! Every constant in src/abi.rs is checked by abi_check.c, which clang
//! compiles against the SDK (`clang -fsyntax-only abi_check.c`): each
//! four-character code has a SAME() line with the same name and code, each
//! other scalar constant a SAME() line, and each UUID a SAME_UUID() line
//! with the same bytes.

use std::collections::BTreeMap;
use std::path::Path;

fn read(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `pub const NAME: TYPE = EXPR;` items of abi.rs as (name, expr), with the
/// expression's whitespace collapsed.
fn rust_consts(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in src.split("pub const ").skip(1) {
        // The item ends at the first `;` outside brackets (`[u8; 16]` has one).
        let mut depth = 0i32;
        let Some(end) = item.find(|c| {
            match c {
                '[' | '(' | '{' => depth += 1,
                ']' | ')' | '}' => depth -= 1,
                _ => {}
            }
            c == ';' && depth == 0
        }) else {
            continue;
        };
        let item: String = item[..end].split_whitespace().collect::<Vec<_>>().join(" ");
        let Some((head, expr)) = item.split_once(" = ") else { continue };
        let Some((name, _ty)) = head.split_once(':') else { continue };
        if name.contains('(') {
            continue; // `pub const fn`
        }
        out.push((name.trim().to_owned(), expr.trim().to_owned()));
    }
    out
}

/// The code in `fourcc(b"abcd")`, if `expr` is one.
fn fourcc_of(expr: &str) -> Option<String> {
    let start = expr.find("fourcc(b\"")? + "fourcc(b\"".len();
    let code = expr.get(start..start + 4)?;
    (expr.get(start + 4..start + 6) == Some("\")")).then(|| code.to_owned())
}

/// The arguments of every `MACRO(` line in abi_check.c, split at commas.
fn c_calls(src: &str, macro_name: &str) -> Vec<Vec<String>> {
    let prefix = format!("{macro_name}(");
    src.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix(&prefix))
        .filter_map(|rest| rest.rsplit_once(')'))
        .map(|(args, _)| args.split(',').map(|a| a.trim().to_owned()).collect())
        .collect()
}

#[test]
fn every_abi_constant_is_checked_against_the_sdk() {
    let rust = read("src/abi.rs");
    let c = read("abi_check.c");
    let consts = rust_consts(&rust);
    assert!(consts.len() > 100, "parsed only {} constants", consts.len());

    let same: BTreeMap<String, String> = c_calls(&c, "SAME")
        .into_iter()
        .filter(|args| args.len() == 2)
        .map(|args| (args[0].clone(), args[1].clone()))
        .collect();
    let uuids: BTreeMap<String, (String, String)> = c_calls(&c, "SAME_UUID")
        .into_iter()
        .filter(|args| args.len() == 3)
        .map(|args| (args[0].clone(), (args[1].clone(), args[2].clone())))
        .collect();

    let mut fourccs = 0;
    let mut problems = Vec::new();
    for (name, expr) in &consts {
        if let Some(code) = fourcc_of(expr) {
            fourccs += 1;
            match same.get(name) {
                Some(v) if *v == format!("'{code}'") => {}
                Some(v) => problems.push(format!("{name}: abi.rs has '{code}', abi_check.c {v}")),
                None => problems.push(format!("{name} ('{code}') has no SAME() line")),
            }
        } else if expr.starts_with('[') {
            let bytes: Vec<u8> = expr
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(|b| u8::from_str_radix(b.trim_start_matches("0x"), 16).expect(b))
                .collect();
            assert_eq!(bytes.len(), 16, "{name}");
            let hex = |b: &[u8]| -> String {
                format!("0x{}", b.iter().map(|x| format!("{x:02X}")).collect::<String>())
            };
            match uuids.get(name) {
                Some((hi, lo)) if *hi == hex(&bytes[..8]) && *lo == hex(&bytes[8..]) => {}
                Some(v) => problems.push(format!("{name}: abi_check.c has {v:?}")),
                None => problems.push(format!("{name} has no SAME_UUID() line")),
            }
        } else if name != "PTR" && !same.contains_key(name) {
            problems.push(format!("{name} ({expr}) has no SAME() line"));
        }
    }
    assert!(problems.is_empty(), "abi.rs and abi_check.c disagree:\n{}", problems.join("\n"));

    // No four-character code hides outside a named constant.
    let total = rust.matches("fourcc(b\"").count();
    assert_eq!(total, fourccs, "fourcc() used outside `pub const` items");
    assert!(fourccs >= 80, "parsed only {fourccs} four-character codes");
}

#[test]
fn io_operations_are_the_sdk_codes() {
    use ovsc_hal::abi::*;
    // The two that matter most, and the easiest to get wrong (design F8).
    assert_eq!(kAudioServerPlugInIOOperationReadInput, u32::from_be_bytes(*b"read"));
    assert_eq!(kAudioServerPlugInIOOperationWriteMix, u32::from_be_bytes(*b"rite"));
}

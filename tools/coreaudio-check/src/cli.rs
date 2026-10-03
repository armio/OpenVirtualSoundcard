//! Argument parsing: positional arguments, `--flag value` options and bare
//! `--switch`es.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

/// Options that take no value.
const SWITCHES: &[&str] = &["--expect-silent-input"];

#[derive(Debug, Default)]
pub struct Args {
    pub positional: Vec<String>,
    options: BTreeMap<String, String>,
    switches: BTreeSet<String>,
}

impl Args {
    pub fn parse(args: &[String]) -> Result<Args, String> {
        let mut out = Args::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if SWITCHES.contains(&a.as_str()) {
                out.switches.insert(a.clone());
            } else if a.starts_with("--") {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                out.options.insert(a.clone(), v.clone());
            } else {
                out.positional.push(a.clone());
            }
        }
        Ok(out)
    }

    /// Fails on options not in `known` (switches included).
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        for k in self.options.keys().chain(self.switches.iter()) {
            if !known.contains(&k.as_str()) {
                return Err(format!("unknown option {k}"));
            }
        }
        Ok(())
    }

    pub fn get<T: FromStr>(&self, flag: &str) -> Result<Option<T>, String> {
        match self.options.get(flag) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("bad value for {flag}: {v:?}")),
        }
    }

    pub fn str(&self, flag: &str) -> Option<&str> {
        self.options.get(flag).map(String::as_str)
    }

    pub fn switch(&self, flag: &str) -> bool {
        self.switches.contains(flag)
    }

    /// The `i`th positional argument, parsed.
    pub fn pos<T: FromStr>(&self, i: usize, what: &str) -> Result<Option<T>, String> {
        match self.positional.get(i) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("bad {what}: {v:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_mixed_arguments() {
        let a =
            Args::parse(&args("uid1 --seconds 2.5 --expect-silent-input --rate 48000 30")).unwrap();
        assert_eq!(a.positional, vec!["uid1", "30"]);
        assert_eq!(a.get::<f64>("--seconds").unwrap(), Some(2.5));
        assert_eq!(a.get::<u32>("--rate").unwrap(), Some(48_000));
        assert!(a.switch("--expect-silent-input"));
        assert_eq!(a.pos::<u64>(1, "seconds").unwrap(), Some(30));
        assert!(a.only(&["--seconds", "--rate", "--expect-silent-input"]).is_ok());
        assert!(a.only(&["--seconds"]).is_err());
        assert!(a.get::<u32>("--seconds").is_err());
        assert!(Args::parse(&args("--rate")).is_err());
    }
}
